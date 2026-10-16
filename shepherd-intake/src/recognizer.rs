//! Form recognition worker bound to the `intake-recognizer` durable consumer.
//!
//! Triggers: `shepherd.storage.document.created.*` (automatic) and
//! `shepherd.intake.recognition.requested.*` (manual, optionally with a form type). The form
//! type hint comes from the request, else the document label `form_type`, else classification.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_nats::jetstream::{self, AckKind, Message};
use futures::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use shepherd_common::{
    consumer::{dead_letter, delivered, retry_or_dead_letter},
    events::{
        DOCUMENT_CREATED, EVENTS_STREAM, Envelope, RECOGNITION_COMPLETED, RECOGNITION_FAILED,
        RECOGNITION_REQUESTED, RECOGNITION_STARTED, RECOGNIZER_CONSUMER, RecognitionCompleted,
        RecognitionFailed, RecognitionRequested, RecognitionStarted,
    },
    topology::bind_consumer,
};
use shepherd_forms::recognize::Recognizer;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    SERVICE, form, ir,
    storage::{StorageClient, StorageError},
};

const FETCH_EXPIRES: Duration = Duration::from_secs(2);
const PROGRESS_EVERY: Duration = Duration::from_secs(30);

enum JobError {
    Transient(anyhow::Error),
    Terminal(String),
}

impl From<StorageError> for JobError {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::NotFound => {
                JobError::Terminal("document or artifact not found in storage".into())
            }
            StorageError::Status { status, body } if status.is_client_error() => {
                JobError::Terminal(format!("storage rejected request ({status}): {body}"))
            }
            other => JobError::Transient(other.into()),
        }
    }
}

pub struct RecognizerWorker {
    pub js: jetstream::Context,
    pub storage: StorageClient,
    pub recognizer: Arc<Recognizer>,
    pub htr_model: Option<String>,
    pub concurrency: usize,
    pub timeout: Duration,
    /// `concurrency` permits, each held by a recognition until its blocking task returns. A
    /// timed-out task cannot be cancelled and keeps its permit, so retries cannot oversubscribe.
    pub slots: Arc<tokio::sync::Semaphore>,
}

impl RecognizerWorker {
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) -> anyhow::Result<()> {
        let Some(mut consumer) =
            bind_consumer(&self.js, EVENTS_STREAM, RECOGNIZER_CONSUMER, &shutdown).await
        else {
            return Ok(());
        };
        let max_deliver = consumer.info().await?.config.max_deliver;
        tracing::info!(
            consumer = RECOGNIZER_CONSUMER,
            max_deliver,
            templates = self.recognizer.set.templates.len(),
            "consuming"
        );
        loop {
            let fetch = consumer
                .fetch()
                .max_messages(self.concurrency)
                .expires(FETCH_EXPIRES)
                .messages();
            let batch = tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                b = fetch => b,
            };
            let batch = match batch {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "fetch failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            batch
                .for_each_concurrent(self.concurrency, |m| {
                    let this = self.clone();
                    async move {
                        match m {
                            Ok(m) => this.handle(m, max_deliver).await,
                            Err(e) => tracing::warn!(error = %e, "batch message error"),
                        }
                    }
                })
                .await;
        }
    }

    async fn handle(&self, m: Message, max_deliver: i64) {
        let ev = match serde_json::from_slice::<Envelope<serde_json::Value>>(&m.payload) {
            Ok(ev) => ev,
            Err(e) => {
                return dead_letter(
                    &self.js,
                    RECOGNIZER_CONSUMER,
                    &m,
                    &format!("malformed envelope: {e}"),
                )
                .await;
            }
        };
        let (manual, hint) = match ev.kind.as_str() {
            DOCUMENT_CREATED => (false, None),
            RECOGNITION_REQUESTED => {
                let req: Option<RecognitionRequested> =
                    serde_json::from_value(ev.data.clone()).ok();
                (true, req.and_then(|r| r.form_type))
            }
            _ => {
                let _ = m.ack().await;
                return;
            }
        };
        let doc_id = ev.subject;
        let started = Instant::now();
        match self.process(&m, doc_id, manual, hint).await {
            Ok(Some(done)) => {
                metrics::counter!("recognitions_total", "outcome" => "completed").increment(1);
                metrics::histogram!("recognition_duration_seconds")
                    .record(started.elapsed().as_secs_f64());
                self.emit(
                    RECOGNITION_COMPLETED,
                    doc_id,
                    RecognitionCompleted {
                        duration_ms: started.elapsed().as_millis() as u64,
                        ..done
                    },
                )
                .await;
                let _ = m.ack().await;
            }
            Ok(None) => {
                metrics::counter!("recognitions_total", "outcome" => "skipped").increment(1);
                let _ = m.ack().await;
            }
            Err(JobError::Transient(e)) => {
                let err = format!("{e:#}");
                let attempt = delivered(&m);
                tracing::warn!(error = %err, %doc_id, attempt, "recognition failed; will retry");
                let terminal = max_deliver > 0 && attempt >= max_deliver;
                self.emit(
                    RECOGNITION_FAILED,
                    doc_id,
                    RecognitionFailed {
                        document_id: doc_id,
                        error: err.clone(),
                        attempt,
                        terminal,
                    },
                )
                .await;
                retry_or_dead_letter(&self.js, RECOGNIZER_CONSUMER, &m, &err, max_deliver).await;
            }
            Err(JobError::Terminal(err)) => {
                tracing::error!(error = %err, %doc_id, "recognition failed permanently");
                let attempt = delivered(&m);
                self.emit(
                    RECOGNITION_FAILED,
                    doc_id,
                    RecognitionFailed {
                        document_id: doc_id,
                        error: err.clone(),
                        attempt,
                        terminal: true,
                    },
                )
                .await;
                dead_letter(&self.js, RECOGNIZER_CONSUMER, &m, &err).await;
            }
        }
    }

    async fn process(
        &self,
        m: &Message,
        doc_id: Uuid,
        manual: bool,
        hint: Option<String>,
    ) -> Result<Option<RecognitionCompleted>, JobError> {
        let doc = self.storage.document(doc_id).await?;
        let source = doc
            .source()
            .ok_or_else(|| JobError::Terminal("document has no source artifact".into()))?
            .clone();
        if !manual
            && doc.artifacts.iter().any(|a| {
                a.schema == form::SCHEMA
                    && a.parent_artifact_id == Some(source.id)
                    && a.produced_by.as_deref() == Some(form::PRODUCER)
            })
        {
            return Ok(None);
        }
        let hint = hint.or_else(|| {
            doc.labels
                .get("form_type")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        });
        if let Some(h) = &hint
            && self.recognizer.set.candidates(h).is_empty()
        {
            return Err(JobError::Terminal(format!("unknown form type {h:?}")));
        }
        self.emit(
            RECOGNITION_STARTED,
            doc_id,
            RecognitionStarted {
                document_id: doc_id,
                source_artifact_id: source.id,
                recognizer: form::PRODUCER.into(),
            },
        )
        .await;

        let bytes = self.storage.content(source.id).await?;
        let actual = hex::encode(Sha256::digest(&bytes));
        if actual != source.sha256.trim() {
            return Err(JobError::Terminal(format!(
                "source sha256 mismatch: expected {}, got {actual}",
                source.sha256
            )));
        }
        let rec = self.recognizer.clone();
        // Waiting on a slot held by a timed-out task can outlast ack_wait; keep the message leased.
        let acquire = self.slots.clone().acquire_owned();
        tokio::pin!(acquire);
        let slot = loop {
            tokio::select! {
                s = &mut acquire => break s.expect("recognition slots are never closed"),
                _ = tokio::time::sleep(PROGRESS_EVERY) => {
                    let _ = m.ack_with(AckKind::Progress).await;
                }
            }
        };
        let job = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            rec.recognize(&bytes, hint.as_deref())
        });
        let result = match tokio::time::timeout(self.timeout, job).await {
            Err(_) => {
                metrics::counter!("recognition_timeouts_total").increment(1);
                return Err(JobError::Transient(anyhow::anyhow!(
                    "recognition exceeded {:?}",
                    self.timeout
                )));
            }
            Ok(Err(join)) => return Err(JobError::Terminal(format!("recognizer crashed: {join}"))),
            // pdfium rejects the file: retrying cannot help.
            Ok(Ok(Err(e))) => return Err(JobError::Terminal(format!("{e:#}"))),
            Ok(Ok(Ok(r))) => r,
        };
        let form_type = result.form.as_ref().map(|f| f.form_type().to_owned());
        let template_id = result.form.as_ref().map(|f| f.template_id.clone());
        let stats = result.stats.clone();
        let artifact = form::build(
            ir::Source {
                document_id: doc_id,
                artifact_id: source.id,
                sha256: source.sha256.trim().to_owned(),
                filename: doc.source_filename.clone(),
                size_bytes: source.size_bytes,
            },
            result,
            &self.recognizer.cfg,
            self.htr_model.clone(),
        );
        let body = serde_json::to_vec(&artifact).map_err(|e| JobError::Terminal(e.to_string()))?;
        let art = self
            .storage
            .create_artifact(doc_id, form::SCHEMA, form::PRODUCER, source.id, body)
            .await?;
        tracing::info!(%doc_id, artifact_id = %art.id, form_type = ?form_type, template_id = ?template_id, flagged = stats.requires_receipting, "form recognized");
        Ok(Some(RecognitionCompleted {
            document_id: doc_id,
            source_artifact_id: source.id,
            artifact_id: art.id,
            version: art.version,
            form_type,
            template_id,
            fields: stats.total,
            filled: stats.filled,
            requires_receipting: stats.requires_receipting,
            duration_ms: 0,
        }))
    }

    async fn emit<T: Serialize>(&self, kind: &str, doc_id: Uuid, data: T) {
        let ev = Envelope::new(kind, SERVICE, doc_id, data);
        let res = async {
            let payload = serde_json::to_vec(&ev)?;
            self.js
                .publish(ev.nats_subject(), payload.into())
                .await?
                .await?;
            anyhow::Ok(())
        };
        if let Err(e) = res.await {
            tracing::warn!(error = %e, kind, %doc_id, "status event not published");
        }
    }
}

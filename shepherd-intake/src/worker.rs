//! Extraction worker bound to the `intake-extractor` durable consumer.
//!
//! Triggers: `shepherd.storage.document.created.*` (automatic) and
//! `shepherd.intake.extraction.requested.*` (manual reprocess). Automatic triggers are skipped
//! when an IR from this extractor version already exists for the current source artifact, so
//! redelivery does not create duplicate versions.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_nats::jetstream::{self, Message};
use futures::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use shepherd_common::{
    consumer::{dead_letter, delivered, retry_or_dead_letter},
    events::{
        DOCUMENT_CREATED, EVENTS_STREAM, EXTRACTION_COMPLETED, EXTRACTION_FAILED,
        EXTRACTION_REQUESTED, EXTRACTION_STARTED, Envelope, ExtractionCompleted, ExtractionFailed,
        ExtractionStarted, INTAKE_CONSUMER,
    },
    topology::bind_consumer,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    PRODUCER, SERVICE,
    extract::{self, ExtractError},
    ir,
    storage::{StorageClient, StorageError},
};

const FETCH_EXPIRES: Duration = Duration::from_secs(2);

enum JobError {
    /// Retrying may succeed (storage unavailable, timeouts).
    Transient(anyhow::Error),
    /// Retrying cannot succeed (unreadable PDF, missing document, hash mismatch).
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

enum Trigger {
    Auto,
    Manual,
}

pub struct Worker {
    pub js: jetstream::Context,
    pub storage: StorageClient,
    pub concurrency: usize,
    pub extract_timeout: Duration,
    pub max_page_chars: usize,
}

impl Worker {
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) -> anyhow::Result<()> {
        let Some(mut consumer) =
            bind_consumer(&self.js, EVENTS_STREAM, INTAKE_CONSUMER, &shutdown).await
        else {
            return Ok(());
        };
        let max_deliver = consumer.info().await?.config.max_deliver;
        tracing::info!(consumer = INTAKE_CONSUMER, max_deliver, "consuming");
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
                    INTAKE_CONSUMER,
                    &m,
                    &format!("malformed envelope: {e}"),
                )
                .await;
            }
        };
        let trigger = match ev.kind.as_str() {
            DOCUMENT_CREATED => Trigger::Auto,
            EXTRACTION_REQUESTED => Trigger::Manual,
            other => {
                tracing::debug!(kind = other, "ignoring event");
                let _ = m.ack().await;
                return;
            }
        };
        let doc_id = ev.subject;
        let started = Instant::now();
        match self.process(doc_id, trigger).await {
            Ok(done) => {
                if let Some(done) = done {
                    metrics::counter!("extractions_total", "outcome" => "completed").increment(1);
                    metrics::histogram!("extraction_duration_seconds")
                        .record(started.elapsed().as_secs_f64());
                    self.emit(
                        EXTRACTION_COMPLETED,
                        doc_id,
                        ExtractionCompleted {
                            duration_ms: started.elapsed().as_millis() as u64,
                            ..done
                        },
                    )
                    .await;
                } else {
                    metrics::counter!("extractions_total", "outcome" => "skipped").increment(1);
                }
                if let Err(e) = m.ack().await {
                    tracing::warn!(error = %e, %doc_id, "ack failed");
                }
            }
            Err(JobError::Transient(e)) => {
                let err = format!("{e:#}");
                let attempt = delivered(&m);
                tracing::warn!(error = %err, %doc_id, attempt, "extraction failed; will retry");
                metrics::counter!("extractions_total", "outcome" => "retry").increment(1);
                let terminal = max_deliver > 0 && attempt >= max_deliver;
                self.emit(
                    EXTRACTION_FAILED,
                    doc_id,
                    ExtractionFailed {
                        document_id: doc_id,
                        error: err.clone(),
                        attempt,
                        terminal,
                    },
                )
                .await;
                retry_or_dead_letter(&self.js, INTAKE_CONSUMER, &m, &err, max_deliver).await;
            }
            Err(JobError::Terminal(err)) => {
                tracing::error!(error = %err, %doc_id, "extraction failed permanently");
                metrics::counter!("extractions_total", "outcome" => "failed").increment(1);
                let attempt = delivered(&m);
                self.emit(
                    EXTRACTION_FAILED,
                    doc_id,
                    ExtractionFailed {
                        document_id: doc_id,
                        error: err.clone(),
                        attempt,
                        terminal: true,
                    },
                )
                .await;
                dead_letter(&self.js, INTAKE_CONSUMER, &m, &err).await;
            }
        }
    }

    /// Returns `None` when skipped as already done.
    async fn process(
        &self,
        doc_id: Uuid,
        trigger: Trigger,
    ) -> Result<Option<ExtractionCompleted>, JobError> {
        let doc = self.storage.document(doc_id).await?;
        let source = doc
            .source()
            .ok_or_else(|| JobError::Terminal("document has no source artifact".into()))?
            .clone();
        if matches!(trigger, Trigger::Auto)
            && doc.artifacts.iter().any(|a| {
                a.schema == ir::SCHEMA
                    && a.parent_artifact_id == Some(source.id)
                    && a.produced_by.as_deref() == Some(PRODUCER)
            })
        {
            tracing::info!(%doc_id, "IR already present for source; skipping");
            return Ok(None);
        }

        self.emit(
            EXTRACTION_STARTED,
            doc_id,
            ExtractionStarted {
                document_id: doc_id,
                source_artifact_id: source.id,
                extractor: PRODUCER.into(),
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

        let max_chars = self.max_page_chars;
        let job = tokio::task::spawn_blocking(move || extract::extract(&bytes, max_chars));
        // On timeout the blocking thread runs to completion in the background; its result is dropped.
        let extraction = match tokio::time::timeout(self.extract_timeout, job).await {
            Err(_) => {
                return Err(JobError::Transient(anyhow::anyhow!(
                    "extraction exceeded {:?}",
                    self.extract_timeout
                )));
            }
            Ok(Err(join)) => return Err(JobError::Terminal(format!("extractor crashed: {join}"))),
            Ok(Ok(Err(e @ (ExtractError::Invalid(_) | ExtractError::Encrypted)))) => {
                return Err(JobError::Terminal(e.to_string()));
            }
            Ok(Ok(Ok(x))) => x,
        };

        let ir = ir::build(
            ir::Source {
                document_id: doc_id,
                artifact_id: source.id,
                sha256: source.sha256.trim().to_owned(),
                filename: doc.source_filename.clone(),
                size_bytes: source.size_bytes,
            },
            extraction,
        );
        let (pages, text_pages, needs_ocr) =
            (ir.stats.pages, ir.stats.text_pages, ir.flags.needs_ocr);
        let body = serde_json::to_vec(&ir).map_err(|e| JobError::Terminal(e.to_string()))?;
        let art = self
            .storage
            .create_artifact(doc_id, ir::SCHEMA, PRODUCER, source.id, body)
            .await?;
        tracing::info!(%doc_id, artifact_id = %art.id, version = art.version, pages, text_pages, "IR stored");
        Ok(Some(ExtractionCompleted {
            document_id: doc_id,
            source_artifact_id: source.id,
            artifact_id: art.id,
            schema: art.schema,
            version: art.version,
            pages,
            text_pages,
            needs_ocr,
            duration_ms: 0,
        }))
    }

    /// Status events are informational (storage's artifact.created is authoritative); failures are logged.
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

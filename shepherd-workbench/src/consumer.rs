//! Handles `shepherd.intake.recognition.completed.*`: opens receipting tasks and links documents
//! to cases.
//!
//! A task is needed when any field requires receipting or the document matched no template
//! (unless it carries a `receipt_number` label, which marks it as case evidence).
//! Re-recognition of a document with an active task refreshes that task instead of adding one.
//!
//! Case linking, for a document not yet in a case:
//! - `receipt_number` label naming an existing case: attached (as the response while a notice is
//!   outstanding).
//! - Benefit form (`benefits.toml`): a case is opened with the labeled or a generated receipt number.

use std::{sync::Arc, time::Duration};

use async_nats::jetstream::{self, Message};
use futures::StreamExt;
use serde_json::json;
use shepherd_common::{
    consumer::{dead_letter, retry_or_dead_letter},
    events::{
        CASE_OPENED, CaseOpened, EVENTS_STREAM, Envelope, RECEIPTING_CONSUMER, RECEIPTING_REQUIRED,
        ReceiptingRequired, RecognitionCompleted,
    },
    storage_client::StorageClient,
    topology::bind_consumer,
};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::{benefits::Benefits, case_db, case_ops, db, projection::Projector, receipt};

pub struct Consumer {
    pub js: jetstream::Context,
    pub db: PgPool,
    pub storage: StorageClient,
    pub open_unrecognized: bool,
    pub benefits: Arc<Benefits>,
    pub templates: Arc<shepherd_forms::template::TemplateSet>,
    pub projector: Projector,
    pub assigner: crate::workload_ops::Assigner,
    pub receipt_prefix: String,
}

impl Consumer {
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) -> anyhow::Result<()> {
        let Some(mut consumer) =
            bind_consumer(&self.js, EVENTS_STREAM, RECEIPTING_CONSUMER, &shutdown).await
        else {
            return Ok(());
        };
        let max_deliver = consumer.info().await?.config.max_deliver;
        tracing::info!(consumer = RECEIPTING_CONSUMER, "consuming");
        loop {
            let fetch = consumer
                .fetch()
                .max_messages(64)
                .expires(Duration::from_secs(2))
                .messages();
            let batch = tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                b = fetch => b,
            };
            let mut batch = match batch {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "fetch failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            while let Some(m) = batch.next().await {
                match m {
                    Ok(m) => self.handle(m, max_deliver).await,
                    Err(e) => tracing::warn!(error = %e, "batch message error"),
                }
            }
        }
    }

    async fn handle(&self, m: Message, max_deliver: i64) {
        let ev = match serde_json::from_slice::<Envelope<RecognitionCompleted>>(&m.payload) {
            Ok(ev) => ev,
            Err(e) => {
                return dead_letter(
                    &self.js,
                    RECEIPTING_CONSUMER,
                    &m,
                    &format!("malformed event: {e}"),
                )
                .await;
            }
        };
        match self.apply(&ev.data).await {
            Ok(()) => {
                let _ = m.ack().await;
            }
            Err(e) => {
                let err = format!("{e:#}");
                tracing::warn!(error = %err, document_id = %ev.data.document_id, "task update failed");
                retry_or_dead_letter(&self.js, RECEIPTING_CONSUMER, &m, &err, max_deliver).await;
            }
        }
    }

    async fn apply(&self, r: &RecognitionCompleted) -> anyhow::Result<()> {
        let Some(doc) = case_ops::document(&self.storage, r.document_id).await? else {
            // Document removed since recognition; nothing to receipt.
            return Ok(());
        };
        let receipt_label = case_ops::receipt_label(&doc);
        // Events recorded before form editions carry the template id in `form_type`.
        let template = r
            .template_id
            .as_deref()
            .or(r.form_type.as_deref())
            .and_then(|t| self.templates.get(t));
        let form_type = template
            .map(|t| t.form_type().to_owned())
            .or_else(|| r.form_type.clone());
        // A labeled document that is not a known form is evidence for its case, not a form to receipt.
        let wants_task = match &form_type {
            None => self.open_unrecognized && receipt_label.is_none(),
            Some(_) => r.requires_receipting > 0,
        };
        let benefit = form_type.as_deref().and_then(|f| self.benefits.get(f));
        let filename = doc.source_filename.as_deref();
        let form = match benefit {
            Some(_) => case_ops::form_values(&self.storage, &doc).await?,
            None => None,
        };
        let values_template = form.as_ref().and_then(|v| v.template(&self.templates));
        let values = form.as_ref().map(|v| v.values.clone()).unwrap_or_default();

        let mut tx = self.db.begin().await?;
        let task = if wants_task {
            let (task, created) = db::upsert_active(
                &mut tx,
                &db::NewTask {
                    document_id: r.document_id,
                    filename,
                    // Receipting tasks record the template (edition).
                    form_type: template.map(|t| t.id.as_str()).or(r.form_type.as_deref()),
                    kind: "flagged",
                    source_artifact_id: r.source_artifact_id,
                    form_artifact_id: Some(r.artifact_id),
                    flagged_fields: r.requires_receipting as i32,
                    total_fields: r.fields as i32,
                },
            )
            .await?;
            let action = if created {
                "created"
            } else {
                "recognition_updated"
            };
            db::log(
                &mut tx,
                task.id,
                None,
                action,
                json!({ "form_artifact_id": r.artifact_id, "flagged": r.requires_receipting }),
            )
            .await?;
            metrics::counter!("receipting_tasks_total", "action" => action).increment(1);
            Some(task)
        } else {
            None
        };

        // Case linking.
        let mut opened = None;
        let mut touched = None;
        match case_db::for_document(&mut tx, r.document_id).await? {
            Some((c, role)) => {
                // Recognition refreshes facts only until the primary form is receipted.
                if role == "primary"
                    && let Some(b) = benefit.filter(|b| b.form_type == c.form_type)
                    && doc.latest(receipt::SCHEMA).is_none()
                    && case_ops::refresh_facts(&mut tx, &c, b, values_template, &values).await?
                {
                    touched = Some(c.id);
                }
            }
            None => {
                let existing = match &receipt_label {
                    Some(rn) => case_db::by_receipt(&mut tx, rn).await?,
                    None => None,
                };
                if let Some(c) = existing {
                    case_ops::attach(&mut tx, &c, r.document_id, filename, None, None).await?;
                    touched = Some(c.id);
                } else if let Some(b) = benefit {
                    let c = case_ops::open_case(
                        &mut tx,
                        &self.receipt_prefix,
                        &case_ops::Opening {
                            receipt_number: receipt_label.as_deref(),
                            form_type: &b.form_type,
                            document_id: r.document_id,
                            filename,
                            received_date: case_ops::received_date(&doc),
                            facts: &b.derive_facts(values_template, &values),
                            actor: None,
                        },
                    )
                    .await?;
                    metrics::counter!("cases_opened_total").increment(1);
                    touched = Some(c.id);
                    opened = Some(c);
                } else if let Some(rn) = &receipt_label {
                    tracing::info!(document_id = %r.document_id, receipt_number = %rn, "no case for receipt number");
                }
            }
        }
        tx.commit().await?;

        if let Some(id) = touched {
            self.projector.spawn_sync(id);
        }
        if let Some(c) = opened {
            self.assigner.spawn_dispatch(c.id);
            crate::publish(
                &self.js,
                CASE_OPENED,
                r.document_id,
                CaseOpened {
                    case_id: c.id,
                    receipt_number: c.receipt_number,
                    form_type: c.form_type,
                    document_id: r.document_id,
                },
            )
            .await;
        }
        if let Some(task) = task {
            crate::publish(
                &self.js,
                RECEIPTING_REQUIRED,
                r.document_id,
                ReceiptingRequired {
                    document_id: r.document_id,
                    task_id: task.id,
                    form_type: form_type.clone(),
                    flagged_fields: task.flagged_fields,
                },
            )
            .await;
        }
        Ok(())
    }
}

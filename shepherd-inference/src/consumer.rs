//! Durable consumer `inference-rules` on `shepherd.storage.artifact.created.*`: evaluates rules
//! for each `workbench.case.v1` projection; other schemas are acked unread.
//!
//! Evaluation is idempotent (findings are diffed against the stored ones), and projections older
//! than the stored revision are ignored, so redelivery and DLQ replay are safe.
//!
//! Each fetched batch is grouped by subject (the document): groups are handled concurrently, the
//! events of a group in order.

use std::{collections::HashMap, time::Duration};

use async_nats::jetstream::Message;
use futures::StreamExt;
use shepherd_common::{
    consumer::{dead_letter, retry_or_dead_letter},
    events::{ARTIFACT_CREATED, ArtifactCreated, EVENTS_STREAM, Envelope, INFERENCE_CONSUMER},
    topology::bind_consumer,
};
use tokio_util::sync::CancellationToken;

use crate::engine::{CASE_SCHEMA, Engine};

/// The case projection announced by an event; `Ok(None)` for anything else.
pub fn projection(ev: Envelope<serde_json::Value>) -> anyhow::Result<Option<ArtifactCreated>> {
    if ev.kind != ARTIFACT_CREATED {
        return Ok(None);
    }
    let d: ArtifactCreated = serde_json::from_value(ev.data)?;
    Ok((d.schema == CASE_SCHEMA).then_some(d))
}

pub async fn run(e: Engine, concurrency: usize, shutdown: CancellationToken) -> anyhow::Result<()> {
    let Some(mut consumer) =
        bind_consumer(&e.js, EVENTS_STREAM, INFERENCE_CONSUMER, &shutdown).await
    else {
        return Ok(());
    };
    let max_deliver = consumer.info().await?.config.max_deliver;
    tracing::info!(consumer = INFERENCE_CONSUMER, "consuming");
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
            Err(err) => {
                tracing::warn!(error = %err, "fetch failed");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let mut groups: Vec<Vec<Message>> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        while let Some(m) = batch.next().await {
            match m {
                Ok(m) => {
                    let i = *index.entry(m.subject.to_string()).or_insert_with(|| {
                        groups.push(Vec::new());
                        groups.len() - 1
                    });
                    groups[i].push(m);
                }
                Err(err) => tracing::warn!(error = %err, "batch message error"),
            }
        }
        futures::stream::iter(groups)
            .for_each_concurrent(concurrency.max(1), |group| {
                let e = &e;
                async move {
                    for m in group {
                        handle(e, m, max_deliver).await;
                    }
                }
            })
            .await;
    }
}

async fn handle(e: &Engine, m: Message, max_deliver: i64) {
    let d = match serde_json::from_slice::<Envelope<serde_json::Value>>(&m.payload)
        .map_err(anyhow::Error::from)
        .and_then(projection)
    {
        Ok(Some(d)) => d,
        Ok(None) => {
            let _ = m.ack().await;
            return;
        }
        Err(err) => {
            return dead_letter(
                &e.js,
                INFERENCE_CONSUMER,
                &m,
                &format!("malformed event: {err:#}"),
            )
            .await;
        }
    };
    match e.ingest(d.document_id, d.artifact_id).await {
        Ok(()) => {
            let _ = m.ack().await;
        }
        Err(err) if err.downcast_ref::<serde_json::Error>().is_some() => {
            dead_letter(
                &e.js,
                INFERENCE_CONSUMER,
                &m,
                &format!("malformed projection: {err:#}"),
            )
            .await;
        }
        Err(err) => {
            let err = format!("{err:#}");
            tracing::warn!(error = %err, document_id = %d.document_id, "evaluation failed");
            retry_or_dead_letter(&e.js, INFERENCE_CONSUMER, &m, &err, max_deliver).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn env(kind: &str, schema: &str) -> Envelope<serde_json::Value> {
        let doc = Uuid::now_v7();
        let e = Envelope::new(
            kind,
            "test",
            doc,
            ArtifactCreated {
                document_id: doc,
                artifact_id: Uuid::now_v7(),
                schema: schema.into(),
                version: 1,
                sha256: "ab".repeat(32),
            },
        );
        serde_json::from_value(serde_json::to_value(e).unwrap()).unwrap()
    }

    #[test]
    fn selects_case_projections() {
        assert!(
            projection(env(ARTIFACT_CREATED, CASE_SCHEMA))
                .unwrap()
                .is_some()
        );
        assert!(
            projection(env(ARTIFACT_CREATED, "inference.findings.v1"))
                .unwrap()
                .is_none()
        );
        assert!(
            projection(env("shepherd.storage.document.created", CASE_SCHEMA))
                .unwrap()
                .is_none()
        );
    }
}

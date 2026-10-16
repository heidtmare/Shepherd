//! Durable consumer `notification-correspondence`: one letter per case opening (receipt notice),
//! notice issued (RFE, NOID) and decision (approval, denial, withdrawal). Administrative closures
//! get no letter.
//!
//! Letters are keyed by their triggering entity, so redelivery and DLQ replay are no-ops once a
//! letter is recorded. A held letter is not an error; the message is acked and the retry loop
//! picks it up.

use std::time::Duration;

use async_nats::jetstream::{self, Message};
use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::Value;
use shepherd_common::{
    consumer::{dead_letter, retry_or_dead_letter},
    events::{
        CASE_DECIDED, CASE_OPENED, CORRESPONDENCE_CONSUMER, CaseDecided, CaseOpened, EVENTS_STREAM,
        Envelope, NOTICE_ISSUED, NoticeIssued,
    },
    topology::bind_consumer,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{db::NewLetter, letters::Kind, notifier::Notifier};

/// Letter requested by an event.
#[derive(Debug, PartialEq)]
pub struct Trigger {
    pub case_id: Uuid,
    pub receipt_number: String,
    pub document_id: Uuid,
    pub kind: Kind,
    pub source_id: Uuid,
    pub source_artifact_id: Option<Uuid>,
}

fn data<T: DeserializeOwned>(v: Value) -> anyhow::Result<T> {
    Ok(serde_json::from_value(v)?)
}

/// Maps an event to the letter it requests; `Ok(None)` for events that need no letter.
pub fn trigger(ev: Envelope<Value>) -> anyhow::Result<Option<Trigger>> {
    let doc = ev.subject;
    Ok(Some(match ev.kind.as_str() {
        CASE_OPENED => {
            let d: CaseOpened = data(ev.data)?;
            Trigger {
                case_id: d.case_id,
                receipt_number: d.receipt_number,
                document_id: d.document_id,
                kind: Kind::Receipt,
                source_id: d.case_id,
                source_artifact_id: None,
            }
        }
        NOTICE_ISSUED => {
            let d: NoticeIssued = data(ev.data)?;
            let kind = match d.kind.as_str() {
                "rfe" => Kind::Rfe,
                "noid" => Kind::Noid,
                other => anyhow::bail!("unknown notice kind {other:?}"),
            };
            Trigger {
                case_id: d.case_id,
                receipt_number: d.receipt_number,
                document_id: doc,
                kind,
                source_id: d.notice_id,
                source_artifact_id: Some(d.artifact_id),
            }
        }
        CASE_DECIDED => {
            let d: CaseDecided = data(ev.data)?;
            if d.outcome == "closed" {
                return Ok(None);
            }
            let kind = Kind::from_outcome(&d.outcome)
                .ok_or_else(|| anyhow::anyhow!("unknown outcome {:?}", d.outcome))?;
            Trigger {
                case_id: d.case_id,
                receipt_number: d.receipt_number,
                document_id: doc,
                kind,
                source_id: d.artifact_id,
                source_artifact_id: Some(d.artifact_id),
            }
        }
        _ => return Ok(None),
    }))
}

pub async fn run(n: Notifier, shutdown: CancellationToken) -> anyhow::Result<()> {
    let Some(consumer) =
        bind_consumer(&n.js, EVENTS_STREAM, CORRESPONDENCE_CONSUMER, &shutdown).await
    else {
        return Ok(());
    };
    let mut consumer = consumer;
    let max_deliver = consumer.info().await?.config.max_deliver;
    tracing::info!(consumer = CORRESPONDENCE_CONSUMER, "consuming");
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
                Ok(m) => handle(&n, &n.js, m, max_deliver).await,
                Err(e) => tracing::warn!(error = %e, "batch message error"),
            }
        }
    }
}

async fn handle(n: &Notifier, js: &jetstream::Context, m: Message, max_deliver: i64) {
    let t = match serde_json::from_slice::<Envelope<Value>>(&m.payload)
        .map_err(anyhow::Error::from)
        .and_then(trigger)
    {
        Ok(Some(t)) => t,
        Ok(None) => {
            let _ = m.ack().await;
            return;
        }
        Err(e) => {
            return dead_letter(
                js,
                CORRESPONDENCE_CONSUMER,
                &m,
                &format!("malformed event: {e:#}"),
            )
            .await;
        }
    };
    let res = n
        .request(&NewLetter {
            case_id: t.case_id,
            receipt_number: &t.receipt_number,
            document_id: t.document_id,
            kind: t.kind.as_str(),
            source_id: t.source_id,
            source_artifact_id: t.source_artifact_id,
        })
        .await;
    match res {
        Ok(_) => {
            let _ = m.ack().await;
        }
        Err(e) => {
            let err = format!("{e:#}");
            tracing::warn!(error = %err, case_id = %t.case_id, kind = t.kind.as_str(), "letter request failed");
            retry_or_dead_letter(js, CORRESPONDENCE_CONSUMER, &m, &err, max_deliver).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<T: serde::Serialize>(kind: &str, doc: Uuid, d: T) -> Envelope<Value> {
        let e = Envelope::new(kind, "test", doc, d);
        serde_json::from_value(serde_json::to_value(e).unwrap()).unwrap()
    }

    #[test]
    fn maps_events_to_letters() {
        let (doc, case, art) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let t = trigger(env(
            CASE_DECIDED,
            doc,
            CaseDecided {
                case_id: case,
                receipt_number: "SHP2600000001".into(),
                form_type: "uscis.i-765".into(),
                outcome: "denied".into(),
                decided_by: "ana".into(),
                artifact_id: art,
                version: 1,
            },
        ))
        .unwrap()
        .unwrap();
        assert_eq!(t.kind, Kind::Denial);
        assert_eq!((t.document_id, t.source_id), (doc, art));

        let t = trigger(env(
            CASE_OPENED,
            doc,
            CaseOpened {
                case_id: case,
                receipt_number: "SHP2600000001".into(),
                form_type: "uscis.i-765".into(),
                document_id: doc,
            },
        ))
        .unwrap()
        .unwrap();
        assert_eq!((t.kind, t.source_id), (Kind::Receipt, case));

        let closed = CaseDecided {
            case_id: case,
            receipt_number: "SHP2600000001".into(),
            form_type: "uscis.i-765".into(),
            outcome: "closed".into(),
            decided_by: "mia".into(),
            artifact_id: art,
            version: 1,
        };
        assert!(trigger(env(CASE_DECIDED, doc, closed)).unwrap().is_none());

        assert!(
            trigger(env("shepherd.workbench.case.assigned", doc, ()))
                .unwrap()
                .is_none()
        );
    }
}

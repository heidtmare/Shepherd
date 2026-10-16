//! Case correspondence from shepherd-notification.
//!
//! - Proxy: the workbench lists a case's letters, opens their PDFs and retries held letters
//!   through this service (`NOTIFICATION_URL`), so it needs one origin and one auth proxy.
//! - Consumer `workbench-correspondence` on `shepherd.notification.>`: records letter
//!   generation, holds and deliveries in the case history. Entries carry the event id, so
//!   redelivery does not duplicate them.

use std::time::Duration;

use async_nats::jetstream::{self, Message};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::StreamExt;
use serde_json::{Value, json};
use shepherd_common::{
    consumer::{dead_letter, retry_or_dead_letter},
    events::{
        CASE_CORRESPONDENCE_CONSUMER, DELIVERY_FAILED, DELIVERY_SENT, DeliveryFailed, DeliverySent,
        EVENTS_STREAM, Envelope, LETTER_GENERATED, LETTER_HELD, LetterGenerated, LetterHeld,
    },
    topology::bind_consumer,
};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    AppState,
    api::{ApiError, ApiResult, user},
    case_db,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/cases/{id}/letters", get(case_letters))
        .route("/v1/letters/{id}/pdf", get(letter_pdf))
        .route("/v1/letters/{id}/retry", post(retry_letter))
}

fn base(st: &AppState) -> Option<String> {
    st.cfg
        .notification_url
        .as_deref()
        .map(|u| u.trim_end_matches('/').to_owned())
}

fn upstream(e: reqwest::Error) -> ApiError {
    ApiError::Internal(anyhow::anyhow!("notification: {e}"))
}

/// Passes an upstream response through with its status and content type.
pub(crate) async fn relay(r: reqwest::Response) -> ApiResult<Response> {
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut headers = HeaderMap::new();
    for h in [
        header::CONTENT_TYPE,
        header::CONTENT_DISPOSITION,
        header::CACHE_CONTROL,
    ] {
        if let Some(v) = r.headers().get(h.as_str()).and_then(|v| v.to_str().ok()) {
            headers.insert(h, v.parse().map_err(anyhow::Error::from)?);
        }
    }
    let bytes = r.bytes().await.map_err(upstream)?;
    Ok((status, headers, Body::from(bytes)).into_response())
}

/// Letters for the case; `enabled: false` when `NOTIFICATION_URL` is unset.
async fn case_letters(State(st): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    case_db::get(&st.db, id).await?.ok_or(ApiError::NotFound)?;
    let Some(base) = base(&st) else {
        return Ok(Json(json!({ "enabled": false, "letters": [] })).into_response());
    };
    let r = st
        .storage
        .http
        .get(format!("{base}/v1/letters"))
        .query(&[("case_id", id.to_string())])
        .send()
        .await
        .map_err(upstream)?;
    if !r.status().is_success() {
        return relay(r).await;
    }
    let mut v: Value = r.json().await.map_err(upstream)?;
    v["enabled"] = json!(true);
    Ok(Json(v).into_response())
}

async fn letter_pdf(State(st): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let base = base(&st).ok_or(ApiError::NotFound)?;
    let r = st
        .storage
        .http
        .get(format!("{base}/v1/letters/{id}/pdf"))
        .send()
        .await
        .map_err(upstream)?;
    relay(r).await
}

async fn retry_letter(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let actor = user(&headers)?;
    let base =
        base(&st).ok_or_else(|| ApiError::BadRequest("NOTIFICATION_URL not configured".into()))?;
    let r = st
        .storage
        .http
        .post(format!("{base}/v1/letters/{id}/retry"))
        .header("x-user", actor)
        .send()
        .await
        .map_err(upstream)?;
    relay(r).await
}

/// Case history entry for a notification event: `(case_id, action, details)`.
fn entry(ev: &Envelope<Value>) -> anyhow::Result<Option<(Uuid, &'static str, Value)>> {
    let d = ev.data.clone();
    Ok(Some(match ev.kind.as_str() {
        LETTER_GENERATED => {
            let e: LetterGenerated = serde_json::from_value(d)?;
            (
                e.case_id,
                "letter_generated",
                json!({ "letter_id": e.letter_id, "kind": e.kind, "channels": e.channels, "pdf_artifact_id": e.pdf_artifact_id }),
            )
        }
        LETTER_HELD => {
            let e: LetterHeld = serde_json::from_value(d)?;
            (
                e.case_id,
                "letter_held",
                json!({ "letter_id": e.letter_id, "kind": e.kind, "reason": e.reason }),
            )
        }
        DELIVERY_SENT => {
            let e: DeliverySent = serde_json::from_value(d)?;
            (
                e.case_id,
                "letter_sent",
                json!({ "letter_id": e.letter_id, "kind": e.kind, "channel": e.channel, "sent_by": e.sent_by }),
            )
        }
        DELIVERY_FAILED => {
            let e: DeliveryFailed = serde_json::from_value(d)?;
            if !e.terminal {
                return Ok(None);
            }
            (
                e.case_id,
                "letter_failed",
                json!({ "letter_id": e.letter_id, "kind": e.kind, "channel": e.channel, "error": e.error }),
            )
        }
        _ => return Ok(None),
    }))
}

async fn record(db: &PgPool, ev: &Envelope<Value>) -> anyhow::Result<()> {
    let Some((case_id, action, mut details)) = entry(ev)? else {
        return Ok(());
    };
    details["event_id"] = json!(ev.id);
    let mut tx = db.begin().await?;
    if case_db::lock(&mut tx, case_id).await?.is_none() {
        return Ok(());
    }
    let seen: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM workbench.case_events WHERE case_id = $1 AND details->>'event_id' = $2)",
    )
    .bind(case_id)
    .bind(ev.id.to_string())
    .fetch_one(&mut *tx)
    .await?;
    if !seen {
        case_db::log(&mut tx, case_id, None, action, details).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn run(
    js: jetstream::Context,
    db: PgPool,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let Some(mut consumer) =
        bind_consumer(&js, EVENTS_STREAM, CASE_CORRESPONDENCE_CONSUMER, &shutdown).await
    else {
        return Ok(());
    };
    let max_deliver = consumer.info().await?.config.max_deliver;
    tracing::info!(consumer = CASE_CORRESPONDENCE_CONSUMER, "consuming");
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
                Ok(m) => handle(&js, &db, m, max_deliver).await,
                Err(e) => tracing::warn!(error = %e, "batch message error"),
            }
        }
    }
}

async fn handle(js: &jetstream::Context, db: &PgPool, m: Message, max_deliver: i64) {
    let ev = match serde_json::from_slice::<Envelope<Value>>(&m.payload) {
        Ok(ev) => ev,
        Err(e) => {
            return dead_letter(
                js,
                CASE_CORRESPONDENCE_CONSUMER,
                &m,
                &format!("malformed event: {e}"),
            )
            .await;
        }
    };
    match record(db, &ev).await {
        Ok(()) => {
            let _ = m.ack().await;
        }
        Err(e) => {
            let err = format!("{e:#}");
            tracing::warn!(error = %err, kind = ev.kind, "correspondence history failed");
            retry_or_dead_letter(js, CASE_CORRESPONDENCE_CONSUMER, &m, &err, max_deliver).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_entries() {
        let case = Uuid::now_v7();
        let ev = |kind: &str, data: Value| Envelope::new(kind, "t", Uuid::now_v7(), data);
        let failed = |terminal| {
            json!({ "letter_id": Uuid::nil(), "delivery_id": Uuid::nil(), "case_id": case,
                    "receipt_number": "SHP2600000001", "kind": "receipt", "channel": "email",
                    "error": "refused", "attempt": 1, "terminal": terminal })
        };
        assert!(
            entry(&ev(DELIVERY_FAILED, failed(false)))
                .unwrap()
                .is_none()
        );
        let (c, action, _) = entry(&ev(DELIVERY_FAILED, failed(true))).unwrap().unwrap();
        assert_eq!((c, action), (case, "letter_failed"));
        assert!(entry(&ev("shepherd.x.y.z", json!({}))).unwrap().is_none());
    }
}

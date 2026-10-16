//! Server-sent events feed over `SHEPHERD_EVENTS`.
//!
//! Each connection creates an ephemeral ordered consumer, which the server discards once the
//! client disconnects. The SSE `id` is the stream sequence, so browsers resume with
//! `Last-Event-ID` after a reconnect.

use std::convert::Infallible;

use async_nats::jetstream::consumer::{DeliverPolicy, pull::OrderedConfig};
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use shepherd_common::events::{DLQ_PREFIX, EVENTS_STREAM, document_filter};
use utoipa::IntoParams;
use uuid::Uuid;

use crate::{
    AppState,
    error::{ApiError, ApiResult, internal},
};

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum StartFrom {
    /// Replay from the first retained event.
    Start,
    /// Only events published after connecting.
    New,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct EventsParams {
    /// All events for one document (`shepherd.*.*.*.<id>`). Defaults `from` to `start`.
    document_id: Option<Uuid>,
    /// NATS subject filter under `shepherd.` (wildcards allowed). Defaults to `shepherd.>`.
    subject: Option<String>,
    /// `start` or `new`. Ignored when `Last-Event-ID` is sent.
    from: Option<StartFrom>,
}

fn valid_subject(s: &str) -> bool {
    s.starts_with("shepherd.")
        && !s.starts_with(DLQ_PREFIX)
        && s.len() <= 256
        && s.split('.').all(|t| !t.is_empty())
        && s.bytes().all(|b| b.is_ascii_graphic())
}

/// Live event feed as `text/event-stream`. `data` is the event envelope JSON.
#[utoipa::path(get, path = "/v1/events", params(EventsParams), responses((status = 200, content_type = "text/event-stream")))]
pub async fn events(
    State(st): State<AppState>,
    Query(p): Query<EventsParams>,
    headers: HeaderMap,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let (filter, default_from) = match (p.document_id, p.subject) {
        (Some(id), None) => (document_filter(id), StartFrom::Start),
        (None, Some(s)) if valid_subject(&s) => (s, StartFrom::New),
        (None, Some(_)) => {
            return Err(ApiError::BadRequest(
                "subject must be under shepherd. (not dlq)".into(),
            ));
        }
        (None, None) => ("shepherd.>".to_owned(), StartFrom::New),
        (Some(_), Some(_)) => {
            return Err(ApiError::BadRequest(
                "use document_id or subject, not both".into(),
            ));
        }
    };
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let deliver_policy = match (resume, p.from.unwrap_or(default_from)) {
        (Some(seq), _) => DeliverPolicy::ByStartSequence {
            start_sequence: seq + 1,
        },
        (None, StartFrom::Start) => DeliverPolicy::All,
        (None, StartFrom::New) => DeliverPolicy::New,
    };

    let stream = st.js.get_stream(EVENTS_STREAM).await.map_err(internal)?;
    let consumer = stream
        .create_consumer(OrderedConfig {
            filter_subject: filter,
            deliver_policy,
            ..Default::default()
        })
        .await
        .map_err(internal)?;
    let messages = consumer.messages().await.map_err(internal)?;

    let guard = ClientGuard::new();
    let events = messages
        .take_while(|m| {
            let ok = m.is_ok();
            if let Err(e) = m {
                tracing::warn!(error = %e, "event feed closed");
            }
            std::future::ready(ok)
        })
        .filter_map(move |m| {
            let _keep = &guard;
            let m = m.expect("errors filtered by take_while");
            let mut ev = Event::default().data(String::from_utf8_lossy(&m.payload));
            if let Ok(info) = m.info() {
                ev = ev.id(info.stream_sequence.to_string());
            }
            std::future::ready(Some(Ok(ev)))
        });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}

/// Tracks connected SSE clients in the `sse_clients` gauge.
struct ClientGuard;

impl ClientGuard {
    fn new() -> Self {
        metrics::gauge!("sse_clients").increment(1.0);
        Self
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        metrics::gauge!("sse_clients").decrement(1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::valid_subject;

    #[test]
    fn subject_filter_validation() {
        assert!(valid_subject("shepherd.>"));
        assert!(valid_subject("shepherd.intake.*.*.*"));
        assert!(!valid_subject("shepherd.dlq.>"));
        assert!(!valid_subject("$JS.API.>"));
        assert!(!valid_subject("shepherd..x"));
        assert!(!valid_subject("shepherd.a b"));
    }
}

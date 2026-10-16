use async_nats::{HeaderMap as NatsHeaders, jetstream::stream::DirectGetErrorKind};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shepherd_common::{
    consumer::{
        CONSUMER_HEADER, ERROR_HEADER, ORIGINAL_SEQUENCE_HEADER, ORIGINAL_SUBJECT_HEADER,
        REPLAYED_FROM_HEADER,
    },
    events::{DLQ_PREFIX, DLQ_STREAM},
    http::bearer_matches,
};
use time::OffsetDateTime;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::{
    AppState,
    error::{ApiError, ApiResult, internal},
    sse,
};

#[derive(OpenApi)]
#[openapi(
    info(title = "shepherd-stream"),
    paths(sse::events, topology, list_dlq, replay_dlq, delete_dlq)
)]
struct ApiDoc;

pub fn router(state: AppState) -> Router {
    let origins: Vec<HeaderValue> = state
        .cfg
        .cors_allowed_origins
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| HeaderValue::from_str(s).ok())
        .collect();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([Method::GET, Method::POST, Method::DELETE])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::HeaderName::from_static("last-event-id"),
        ]);
    let x_request_id = header::HeaderName::from_static("x-request-id");
    Router::new()
        .route("/v1/events", get(sse::events))
        .route("/v1/topology", get(topology))
        .route("/v1/dlq", get(list_dlq))
        .route("/v1/dlq/{seq}/replay", post(replay_dlq))
        .route("/v1/dlq/{seq}", delete(delete_dlq))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route(
            "/metrics",
            get(|State(st): State<AppState>| async move { st.metrics.render() }),
        )
        .route("/openapi.json", get(|| async { Json(ApiDoc::openapi()) }))
        .layer(middleware::from_fn(shepherd_common::http::track_metrics))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        .layer(SetRequestIdLayer::new(x_request_id, MakeRequestUuid))
        .with_state(state)
}

async fn readyz(State(st): State<AppState>) -> (StatusCode, Json<Value>) {
    let nats = st.nats.connection_state() == async_nats::connection::State::Connected;
    let status = if nats {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(json!({ "nats": nats })))
}

#[derive(Serialize, ToSchema)]
pub struct StreamStatus {
    name: String,
    subjects: Vec<String>,
    messages: u64,
    bytes: u64,
    first_sequence: u64,
    last_sequence: u64,
    consumers: Vec<ConsumerStatus>,
}

#[derive(Serialize, ToSchema)]
pub struct ConsumerStatus {
    name: String,
    filter_subjects: Vec<String>,
    /// Messages not yet delivered.
    num_pending: u64,
    /// Delivered but not yet acked.
    num_ack_pending: usize,
    num_redelivered: usize,
    max_deliver: i64,
}

/// Declared streams and consumers with live counters.
#[utoipa::path(get, path = "/v1/topology", responses((status = 200, body = Vec<StreamStatus>)))]
async fn topology(State(st): State<AppState>) -> ApiResult<Json<Vec<StreamStatus>>> {
    let mut out = Vec::new();
    for def in &st.topology.streams {
        let mut stream = st.js.get_stream(&def.name).await.map_err(internal)?;
        let info = stream.info().await.map_err(internal)?.clone();
        let mut consumers = Vec::new();
        for c in st
            .topology
            .consumers
            .iter()
            .filter(|c| c.stream == def.name)
        {
            let ci = stream.consumer_info(&c.name).await.map_err(internal)?;
            consumers.push(ConsumerStatus {
                name: ci.name.clone(),
                filter_subjects: c.filter_subjects.clone(),
                num_pending: ci.num_pending,
                num_ack_pending: ci.num_ack_pending,
                num_redelivered: ci.num_redelivered,
                max_deliver: ci.config.max_deliver,
            });
        }
        out.push(StreamStatus {
            name: def.name.clone(),
            subjects: info.config.subjects.clone(),
            messages: info.state.messages,
            bytes: info.state.bytes,
            first_sequence: info.state.first_sequence,
            last_sequence: info.state.last_sequence,
            consumers,
        });
    }
    Ok(Json(out))
}

#[derive(Deserialize, IntoParams)]
pub struct DlqParams {
    /// Return entries with sequence greater than this.
    after: Option<u64>,
    /// 1..=500, default 100.
    limit: Option<usize>,
}

#[derive(Serialize, ToSchema)]
pub struct DlqEntry {
    sequence: u64,
    subject: String,
    #[serde(with = "time::serde::rfc3339")]
    time: OffsetDateTime,
    consumer: Option<String>,
    error: Option<String>,
    original_subject: Option<String>,
    original_sequence: Option<u64>,
    /// Parsed JSON when the payload is JSON, otherwise a UTF-8 (lossy) string.
    #[schema(value_type = Object)]
    payload: Value,
}

#[derive(Serialize, ToSchema)]
pub struct DlqPage {
    items: Vec<DlqEntry>,
    next_after: Option<u64>,
}

fn header(h: &NatsHeaders, name: &str) -> Option<String> {
    h.get(name).map(|v| v.as_str().to_owned())
}

/// Dead-lettered messages, oldest first.
#[utoipa::path(get, path = "/v1/dlq", params(DlqParams), responses((status = 200, body = DlqPage)))]
async fn list_dlq(
    State(st): State<AppState>,
    Query(p): Query<DlqParams>,
) -> ApiResult<Json<DlqPage>> {
    let limit = p.limit.unwrap_or(100).clamp(1, 500);
    let mut stream = st.js.get_stream(DLQ_STREAM).await.map_err(internal)?;
    let state = stream.info().await.map_err(internal)?.state.clone();
    let mut items = Vec::new();
    let mut seq = p
        .after
        .map_or(state.first_sequence, |a| a + 1)
        .max(state.first_sequence);
    while seq <= state.last_sequence && items.len() < limit {
        match stream.direct_get(seq).await {
            Ok(m) => items.push(DlqEntry {
                sequence: m.sequence,
                subject: m.subject.to_string(),
                time: m.time,
                consumer: header(&m.headers, CONSUMER_HEADER),
                error: header(&m.headers, ERROR_HEADER),
                original_subject: header(&m.headers, ORIGINAL_SUBJECT_HEADER),
                original_sequence: header(&m.headers, ORIGINAL_SEQUENCE_HEADER)
                    .and_then(|s| s.parse().ok()),
                payload: serde_json::from_slice(&m.payload).unwrap_or_else(|_| {
                    Value::String(String::from_utf8_lossy(&m.payload).into_owned())
                }),
            }),
            // Deleted (replayed or dropped) sequences leave gaps.
            Err(e) if e.kind() == DirectGetErrorKind::NotFound => {}
            Err(e) => return Err(internal(e)),
        }
        seq += 1;
    }
    let next_after = (seq <= state.last_sequence).then(|| seq - 1);
    Ok(Json(DlqPage { items, next_after }))
}

#[derive(Serialize, ToSchema)]
pub struct Replayed {
    subject: String,
    sequence: u64,
}

fn authorize(st: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    if bearer_matches(headers, st.cfg.admin_token.as_deref()) {
        Ok(())
    } else {
        Err(ApiError::Unauthorized)
    }
}

/// Republishes a dead-lettered message to its original subject, then removes it from the DLQ.
/// Every consumer whose filter matches receives it again; consumers must be idempotent.
#[utoipa::path(post, path = "/v1/dlq/{seq}/replay", responses((status = 202, body = Replayed), (status = 401), (status = 404)))]
async fn replay_dlq(
    State(st): State<AppState>,
    Path(seq): Path<u64>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, Json<Replayed>)> {
    authorize(&st, &headers)?;
    let stream = st.js.get_stream(DLQ_STREAM).await.map_err(internal)?;
    let m = match stream.direct_get(seq).await {
        Ok(m) => m,
        Err(e) if e.kind() == DirectGetErrorKind::NotFound => return Err(ApiError::NotFound),
        Err(e) => return Err(internal(e)),
    };
    let subject = header(&m.headers, ORIGINAL_SUBJECT_HEADER)
        .filter(|s| s.starts_with("shepherd.") && !s.starts_with(DLQ_PREFIX))
        .ok_or_else(|| ApiError::BadRequest("entry has no replayable original subject".into()))?;
    let mut out = NatsHeaders::new();
    out.insert(REPLAYED_FROM_HEADER, seq.to_string().as_str());
    let ack = st
        .js
        .publish_with_headers(subject.clone(), out, m.payload.clone())
        .await
        .map_err(internal)?
        .await
        .map_err(internal)?;
    stream.delete_message(seq).await.map_err(internal)?;
    metrics::counter!("dlq_replayed_total").increment(1);
    tracing::info!(seq, subject, "dlq entry replayed");
    Ok((
        StatusCode::ACCEPTED,
        Json(Replayed {
            subject,
            sequence: ack.sequence,
        }),
    ))
}

#[utoipa::path(delete, path = "/v1/dlq/{seq}", responses((status = 204), (status = 401), (status = 404)))]
async fn delete_dlq(
    State(st): State<AppState>,
    Path(seq): Path<u64>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    authorize(&st, &headers)?;
    let stream = st.js.get_stream(DLQ_STREAM).await.map_err(internal)?;
    match stream.delete_message(seq).await {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err(ApiError::NotFound),
        Err(e) if e.to_string().contains("no message found") => Err(ApiError::NotFound),
        Err(e) => Err(internal(e)),
    }
}

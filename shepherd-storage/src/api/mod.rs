mod artifacts;
mod documents;
mod health;

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    extract::DefaultBodyLimit,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use shepherd_common::telemetry::PrometheusHandle;
use sqlx::PgPool;
use tokio::sync::Notify;
use tower_http::{
    compression::{
        CompressionLayer, Predicate,
        predicate::{DefaultPredicate, NotForContentType},
    },
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa::OpenApi;

use crate::{
    blob::BlobStore,
    config::Config,
    db::Artifact,
    error::{ApiError, ApiResult},
};

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: PgPool,
    pub blobs: BlobStore,
    pub relay_wake: Arc<Notify>,
    pub nats: Option<async_nats::Client>,
    pub metrics: PrometheusHandle,
}

#[derive(OpenApi)]
#[openapi(
    info(title = "shepherd-storage"),
    paths(
        documents::create_document,
        documents::list_documents,
        documents::get_document,
        documents::get_source,
        documents::get_audit,
        artifacts::create_artifact,
        artifacts::get_artifact,
        artifacts::get_artifact_content,
    )
)]
struct ApiDoc;

pub fn router(state: AppState) -> Router {
    let json_limit = state.cfg.max_json_bytes;
    let x_request_id = header::HeaderName::from_static("x-request-id");
    Router::new()
        .route(
            "/v1/documents",
            post(documents::create_document).get(documents::list_documents),
        )
        .route("/v1/documents/{id}", get(documents::get_document))
        .route("/v1/documents/{id}/source", get(documents::get_source))
        .route("/v1/documents/{id}/audit", get(documents::get_audit))
        .route(
            "/v1/documents/{id}/artifacts",
            post(artifacts::create_artifact).layer(DefaultBodyLimit::max(json_limit)),
        )
        .route("/v1/artifacts/{id}", get(artifacts::get_artifact))
        .route(
            "/v1/artifacts/{id}/content",
            get(artifacts::get_artifact_content),
        )
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .route("/metrics", get(health::metrics))
        .route(
            "/openapi.json",
            get(|| async { axum::Json(ApiDoc::openapi()) }),
        )
        .layer(middleware::from_fn(shepherd_common::http::track_metrics))
        .layer(
            CompressionLayer::new().compress_when(
                DefaultPredicate::new().and(NotForContentType::new("application/pdf")),
            ),
        )
        .layer(TraceLayer::new_for_http())
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        .layer(SetRequestIdLayer::new(x_request_id, MakeRequestUuid))
        .with_state(state)
}

pub(crate) fn actor(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Streams an artifact blob. Supports a single `bytes=` range and `If-None-Match` on the sha256 ETag.
pub(crate) async fn serve_blob(
    st: &AppState,
    a: &Artifact,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    let etag = format!("\"{}\"", a.sha256);
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"))
    {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }

    let size = a.size_bytes as u64;
    let range = match headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        Some(r) => Some(parse_range(r, size).ok_or(ApiError::RangeNotSatisfiable { size })?),
        None => None,
    };
    let res = st.blobs.get(&a.object_key, range.clone()).await?;
    let len = res.range.end - res.range.start;

    let mut resp = Response::new(Body::from_stream(res.into_stream()));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&a.content_type).map_err(anyhow::Error::from)?,
    );
    h.insert(header::CONTENT_LENGTH, len.into());
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).map_err(anyhow::Error::from)?,
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    if let Some(r) = range {
        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
        resp.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {}-{}/{size}", r.start, r.end - 1))
                .map_err(anyhow::Error::from)?,
        );
    }
    Ok(resp)
}

/// Parses a single-range `Range` header into a half-open byte range clamped to `size`.
/// Multi-range requests are rejected.
fn parse_range(value: &str, size: u64) -> Option<std::ops::Range<u64>> {
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || size == 0 {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let (start, end) = match (start.trim(), end.trim()) {
        ("", suffix) => {
            let n: u64 = suffix.parse().ok()?;
            if n == 0 {
                return None;
            }
            (size.saturating_sub(n), size)
        }
        (s, "") => (s.parse().ok()?, size),
        (s, e) => {
            let e: u64 = e.parse().ok()?;
            (s.parse().ok()?, e.saturating_add(1).min(size))
        }
    };
    (start < end && start < size).then_some(start..end)
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-9", 100), Some(0..10));
        assert_eq!(parse_range("bytes=90-", 100), Some(90..100));
        assert_eq!(parse_range("bytes=-10", 100), Some(90..100));
        assert_eq!(parse_range("bytes=-500", 100), Some(0..100));
        assert_eq!(parse_range("bytes=50-5000", 100), Some(50..100));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=9-3", 100), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 100), None);
        assert_eq!(parse_range("items=0-1", 100), None);
        assert_eq!(parse_range("bytes=-0", 100), None);
    }
}

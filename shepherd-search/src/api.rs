use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    routing::{get, post},
};
use serde_json::{Value, json};
use shepherd_common::telemetry::PrometheusHandle;
use tower_http::{
    compression::CompressionLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa::OpenApi;
use uuid::Uuid;

use crate::{
    backend::SearchBackend,
    error::{ApiError, ApiResult},
    indexer::Indexer,
    model::{CardView, SearchRequest, SearchResponse},
    query::Plan,
    rebuild::{RebuildStatus, Rebuilder},
};

pub struct AppState<B> {
    pub indexer: Arc<Indexer<B>>,
    pub rebuilder: Arc<Rebuilder>,
    pub nats: async_nats::Client,
    pub admin_token: Option<String>,
    pub metrics: PrometheusHandle,
}

impl<B> Clone for AppState<B> {
    fn clone(&self) -> Self {
        Self {
            indexer: self.indexer.clone(),
            rebuilder: self.rebuilder.clone(),
            nats: self.nats.clone(),
            admin_token: self.admin_token.clone(),
            metrics: self.metrics.clone(),
        }
    }
}

#[derive(OpenApi)]
#[openapi(
    info(title = "shepherd-search"),
    paths(search, get_card, start_rebuild, rebuild_status)
)]
struct ApiDoc;

pub fn router<B: SearchBackend>(state: AppState<B>) -> Router {
    let x_request_id = header::HeaderName::from_static("x-request-id");
    let mut r = Router::new()
        .route("/v1/search", post(search::<B>))
        .route("/v1/cards/{id}", get(get_card::<B>))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz::<B>))
        .route(
            "/metrics",
            get(|State(st): State<AppState<B>>| async move { st.metrics.render() }),
        )
        .route("/openapi.json", get(|| async { Json(ApiDoc::openapi()) }));
    if state.admin_token.is_some() {
        r = r.route(
            "/admin/rebuild",
            post(start_rebuild::<B>).get(rebuild_status::<B>),
        );
    }
    r.layer(middleware::from_fn(shepherd_common::http::track_metrics))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        .layer(SetRequestIdLayer::new(x_request_id, MakeRequestUuid))
        .with_state(state)
}

#[utoipa::path(post, path = "/v1/search", request_body = SearchRequest,
    responses((status = 200, body = SearchResponse), (status = 400), (status = 503)))]
async fn search<B: SearchBackend>(
    State(st): State<AppState<B>>,
    Json(req): Json<SearchRequest>,
) -> ApiResult<Json<SearchResponse>> {
    let plan = Plan::new(&req, &st.indexer.mapping.columns)?;
    Ok(Json(st.indexer.backend.search(&plan).await?))
}

#[utoipa::path(get, path = "/v1/cards/{id}", responses((status = 200, body = CardView), (status = 404)))]
async fn get_card<B: SearchBackend>(
    State(st): State<AppState<B>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<CardView>> {
    st.indexer
        .backend
        .get(id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

fn authorize<B>(st: &AppState<B>, headers: &HeaderMap) -> ApiResult<()> {
    if shepherd_common::http::bearer_matches(headers, st.admin_token.as_deref()) {
        Ok(())
    } else {
        Err(ApiError::Unauthorized)
    }
}

/// Re-project every stored document, spread over all search processes. Requires
/// `Authorization: Bearer $ADMIN_TOKEN`.
#[utoipa::path(post, path = "/admin/rebuild", responses((status = 202, body = RebuildStatus), (status = 401), (status = 409)))]
async fn start_rebuild<B: SearchBackend>(
    State(st): State<AppState<B>>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, Json<RebuildStatus>)> {
    authorize(&st, &headers)?;
    if !st.rebuilder.start().await? {
        return Err(ApiError::Conflict("rebuild already running"));
    }
    Ok((StatusCode::ACCEPTED, Json(st.rebuilder.status().await?)))
}

#[utoipa::path(get, path = "/admin/rebuild", responses((status = 200, body = RebuildStatus), (status = 401)))]
async fn rebuild_status<B: SearchBackend>(
    State(st): State<AppState<B>>,
    headers: HeaderMap,
) -> ApiResult<Json<RebuildStatus>> {
    authorize(&st, &headers)?;
    Ok(Json(st.rebuilder.status().await?))
}

async fn readyz<B: SearchBackend>(State(st): State<AppState<B>>) -> (StatusCode, Json<Value>) {
    let db = st.indexer.backend.ping().await;
    let nats = st.nats.connection_state() == async_nats::connection::State::Connected;
    let status = if db && nats {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(json!({ "db": db, "nats": nats })))
}

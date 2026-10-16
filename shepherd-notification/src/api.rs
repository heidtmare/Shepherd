use std::collections::HashMap;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use shepherd_common::storage_client::StorageError;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa::{IntoParams, OpenApi, ToSchema};
use uuid::Uuid;

use crate::{
    AppState,
    db::{self, Delivery, Letter},
    notifier::Outcome,
};

#[derive(OpenApi)]
#[openapi(
    info(title = "shepherd-notification"),
    paths(
        list_letters,
        get_letter,
        letter_pdf,
        retry_letter,
        list_deliveries,
        mark_mailed,
        retry_delivery
    )
)]
struct ApiDoc;

pub fn router(state: AppState) -> Router {
    let x_request_id = header::HeaderName::from_static("x-request-id");
    Router::new()
        .route("/v1/letters", get(list_letters))
        .route("/v1/letters/{id}", get(get_letter))
        .route("/v1/letters/{id}/pdf", get(letter_pdf))
        .route("/v1/letters/{id}/retry", post(retry_letter))
        .route("/v1/deliveries", get(list_deliveries))
        .route("/v1/deliveries/{id}/mailed", post(mark_mailed))
        .route("/v1/deliveries/{id}/retry", post(retry_delivery))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route(
            "/metrics",
            get(|State(st): State<AppState>| async move { st.metrics.render() }),
        )
        .route("/openapi.json", get(|| async { Json(ApiDoc::openapi()) }))
        .layer(middleware::from_fn(shepherd_common::http::track_metrics))
        .layer(TraceLayer::new_for_http())
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        .layer(SetRequestIdLayer::new(x_request_id, MakeRequestUuid))
        .with_state(state)
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Storage(StorageError),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Internal(e.into())
    }
}

impl From<StorageError> for ApiError {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::NotFound => ApiError::NotFound,
            other => ApiError::Storage(other),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = |code: &str, message: String| Json(json!({ "error": code, "message": message }));
        match self {
            ApiError::BadRequest(m) => {
                (StatusCode::BAD_REQUEST, body("bad_request", m)).into_response()
            }
            ApiError::NotFound => {
                (StatusCode::NOT_FOUND, body("not_found", "not found".into())).into_response()
            }
            ApiError::Conflict(m) => (StatusCode::CONFLICT, body("conflict", m)).into_response(),
            ApiError::Storage(e) => {
                tracing::error!(error = %e, "storage call failed");
                (
                    StatusCode::BAD_GATEWAY,
                    body("upstream", "storage unavailable".into()),
                )
                    .into_response()
            }
            ApiError::Internal(e) => {
                tracing::error!(error = format!("{e:#}"), "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    body("internal", "internal error".into()),
                )
                    .into_response()
            }
        }
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// Acting user from `X-User`. No authentication: deploy behind an auth proxy that sets it.
fn user(headers: &HeaderMap) -> ApiResult<String> {
    headers
        .get("x-user")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|u| !u.is_empty() && u.len() <= 64)
        .map(str::to_owned)
        .ok_or_else(|| ApiError::BadRequest("X-User header required".into()))
}

async fn readyz(State(st): State<AppState>) -> impl IntoResponse {
    let db = sqlx::query("SELECT 1").execute(&st.db).await.is_ok();
    let nats = st.nats.connection_state() == async_nats::connection::State::Connected;
    let storage = st.notifier.storage.ready().await;
    let status = if db && nats && storage {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({ "db": db, "nats": nats, "storage": storage })),
    )
}

#[derive(Serialize, ToSchema)]
struct LetterView {
    #[serde(flatten)]
    letter: Letter,
    deliveries: Vec<Delivery>,
}

async fn with_deliveries(st: &AppState, letters: Vec<Letter>) -> ApiResult<Vec<LetterView>> {
    let ids: Vec<Uuid> = letters.iter().map(|l| l.id).collect();
    let mut by_letter: HashMap<Uuid, Vec<Delivery>> = HashMap::new();
    for d in db::deliveries_for(&st.db, &ids).await? {
        by_letter.entry(d.letter_id).or_default().push(d);
    }
    Ok(letters
        .into_iter()
        .map(|l| LetterView {
            deliveries: by_letter.remove(&l.id).unwrap_or_default(),
            letter: l,
        })
        .collect())
}

#[derive(Deserialize, IntoParams)]
struct LetterQuery {
    case_id: Option<Uuid>,
    /// `held` or `generated`.
    status: Option<String>,
    kind: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize, ToSchema)]
struct LetterList {
    letters: Vec<LetterView>,
}

/// Letters, newest first.
#[utoipa::path(get, path = "/v1/letters", params(LetterQuery), responses((status = 200, body = LetterList)))]
async fn list_letters(
    State(st): State<AppState>,
    Query(q): Query<LetterQuery>,
) -> ApiResult<Json<LetterList>> {
    let letters = db::letters(
        &st.db,
        &db::LetterFilter {
            case_id: q.case_id,
            status: q.status.as_deref(),
            kind: q.kind.as_deref(),
            limit: q.limit.unwrap_or(100).clamp(1, 500),
        },
    )
    .await?;
    Ok(Json(LetterList {
        letters: with_deliveries(&st, letters).await?,
    }))
}

async fn letter(st: &AppState, id: Uuid) -> ApiResult<LetterView> {
    let l = db::get_letter(&st.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(with_deliveries(st, vec![l]).await?.remove(0))
}

#[utoipa::path(get, path = "/v1/letters/{id}", responses((status = 200, body = LetterView), (status = 404)))]
async fn get_letter(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<LetterView>> {
    Ok(Json(letter(&st, id).await?))
}

/// The letter PDF, streamed from storage.
#[utoipa::path(get, path = "/v1/letters/{id}/pdf", responses((status = 200, content_type = "application/pdf"), (status = 404)))]
async fn letter_pdf(State(st): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let l = db::get_letter(&st.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let art = l.pdf_artifact_id.ok_or(ApiError::NotFound)?;
    let bytes = st.notifier.storage.content(art).await?;
    let name = format!("{}-{}.pdf", l.receipt_number, l.kind);
    Ok((
        [
            (header::CONTENT_TYPE, "application/pdf".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{name}\""),
            ),
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable".to_owned(),
            ),
        ],
        Body::from(bytes),
    )
        .into_response())
}

/// Tries to generate a held letter now.
#[utoipa::path(post, path = "/v1/letters/{id}/retry", responses((status = 200, body = LetterView), (status = 404), (status = 409)))]
async fn retry_letter(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<LetterView>> {
    let l = db::get_letter(&st.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if l.status != "held" {
        return Err(ApiError::Conflict(format!("letter is {}", l.status)));
    }
    match st.notifier.process(id).await? {
        Outcome::Generated | Outcome::Held(_) | Outcome::Unchanged => {}
    }
    Ok(Json(letter(&st, id).await?))
}

#[derive(Deserialize, IntoParams)]
struct DeliveryQuery {
    /// `mail` or `email`.
    channel: Option<String>,
    /// `queued`, `sent` or `failed`.
    status: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize, ToSchema)]
struct DeliveryList {
    deliveries: Vec<Delivery>,
}

/// Deliveries, oldest first. `channel=mail&status=queued` is the print-and-mail queue.
#[utoipa::path(get, path = "/v1/deliveries", params(DeliveryQuery), responses((status = 200, body = DeliveryList)))]
async fn list_deliveries(
    State(st): State<AppState>,
    Query(q): Query<DeliveryQuery>,
) -> ApiResult<Json<DeliveryList>> {
    Ok(Json(DeliveryList {
        deliveries: db::deliveries(
            &st.db,
            &db::DeliveryFilter {
                channel: q.channel.as_deref(),
                status: q.status.as_deref(),
                limit: q.limit.unwrap_or(100).clamp(1, 500),
            },
        )
        .await?,
    }))
}

#[derive(Deserialize, ToSchema)]
struct Mailed {
    /// Tracking number or batch reference.
    reference: Option<String>,
}

/// Records that a queued letter was printed and mailed.
#[utoipa::path(post, path = "/v1/deliveries/{id}/mailed", request_body = Mailed,
    responses((status = 200, body = Delivery), (status = 404), (status = 409)))]
async fn mark_mailed(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    body: Option<Json<Mailed>>,
) -> ApiResult<Json<Delivery>> {
    let actor = user(&headers)?;
    let reference = body
        .and_then(|Json(b)| b.reference)
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty());
    let mut tx = st.db.begin().await?;
    let d = db::lock_delivery(&mut tx, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if d.channel != "mail" {
        return Err(ApiError::Conflict("not a mail delivery".into()));
    }
    if d.status != "queued" {
        return Err(ApiError::Conflict(format!("delivery is {}", d.status)));
    }
    let d = db::mark_sent(&mut tx, id, Some(&actor), reference.as_deref()).await?;
    tx.commit().await?;
    st.notifier.delivery_event(&d, Some(&actor)).await?;
    Ok(Json(d))
}

/// Returns a failed e-mail delivery to the queue.
#[utoipa::path(post, path = "/v1/deliveries/{id}/retry", responses((status = 200, body = Delivery), (status = 404), (status = 409)))]
async fn retry_delivery(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> ApiResult<Json<Delivery>> {
    user(&headers)?;
    let mut tx = st.db.begin().await?;
    let d = db::lock_delivery(&mut tx, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if d.status != "failed" {
        return Err(ApiError::Conflict(format!("delivery is {}", d.status)));
    }
    let d = db::requeue(&mut tx, id).await?;
    tx.commit().await?;
    st.notifier.spawn_send();
    Ok(Json(d))
}

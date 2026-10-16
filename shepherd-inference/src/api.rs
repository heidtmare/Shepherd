use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa::{IntoParams, OpenApi, ToSchema};
use uuid::Uuid;

use crate::{
    AppState,
    db::{self, Finding},
    rules::RuleInfo,
};

#[derive(OpenApi)]
#[openapi(
    info(title = "shepherd-inference"),
    paths(list_rules, list_findings, get_finding, review_finding)
)]
struct ApiDoc;

pub fn router(state: AppState) -> Router {
    let x_request_id = header::HeaderName::from_static("x-request-id");
    Router::new()
        .route("/v1/rules", get(list_rules))
        .route("/v1/findings", get(list_findings))
        .route("/v1/findings/{id}", get(get_finding))
        .route("/v1/findings/{id}/review", post(review_finding))
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
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Internal(e.into())
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
    let db = sqlx::query("SELECT 1").execute(&st.engine.db).await.is_ok();
    let nats = st.nats.connection_state() == async_nats::connection::State::Connected;
    let storage = st.engine.storage.ready().await;
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
struct RuleList {
    rules: Vec<RuleInfo>,
    /// Identifies the configuration findings were last fully evaluated with.
    fingerprint: String,
}

/// Configured rules.
#[utoipa::path(get, path = "/v1/rules", responses((status = 200, body = RuleList)))]
async fn list_rules(State(st): State<AppState>) -> Json<RuleList> {
    Json(RuleList {
        rules: st.engine.rules.info(),
        fingerprint: st.engine.fingerprint.clone(),
    })
}

#[derive(Deserialize, IntoParams)]
struct FindingQuery {
    case_id: Option<Uuid>,
    rule: Option<String>,
    /// `open` (default), `resolved` or `all`.
    status: Option<String>,
    /// `none` (unreviewed), `confirmed` or `dismissed`.
    review: Option<String>,
    /// `low`, `medium` or `high`.
    severity: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize, ToSchema)]
struct FindingList {
    findings: Vec<Finding>,
}

fn one_of<'a>(name: &str, v: Option<&'a str>, allowed: &[&str]) -> ApiResult<Option<&'a str>> {
    match v {
        Some(s) if !allowed.contains(&s) => Err(ApiError::BadRequest(format!(
            "{name} must be one of {}",
            allowed.join(", ")
        ))),
        other => Ok(other),
    }
}

/// Findings, newest first.
#[utoipa::path(get, path = "/v1/findings", params(FindingQuery), responses((status = 200, body = FindingList)))]
async fn list_findings(
    State(st): State<AppState>,
    Query(q): Query<FindingQuery>,
) -> ApiResult<Json<FindingList>> {
    let status = match one_of("status", q.status.as_deref(), &["open", "resolved", "all"])? {
        Some("all") => None,
        Some(s) => Some(s),
        None => Some("open"),
    };
    let findings = db::findings(
        &st.engine.db,
        &db::FindingFilter {
            case_id: q.case_id,
            rule: q.rule.as_deref(),
            status,
            review: one_of(
                "review",
                q.review.as_deref(),
                &["none", "confirmed", "dismissed"],
            )?,
            severity: one_of(
                "severity",
                q.severity.as_deref(),
                &["low", "medium", "high"],
            )?,
            limit: q.limit.unwrap_or(100).clamp(1, 500),
        },
    )
    .await?;
    Ok(Json(FindingList { findings }))
}

#[utoipa::path(get, path = "/v1/findings/{id}", responses((status = 200, body = Finding), (status = 404)))]
async fn get_finding(State(st): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<Finding>> {
    Ok(Json(
        db::get_finding(&st.engine.db, id)
            .await?
            .ok_or(ApiError::NotFound)?,
    ))
}

#[derive(Deserialize, ToSchema)]
struct Review {
    /// `confirmed`, `dismissed`, or `null` to clear.
    disposition: Option<String>,
    note: Option<String>,
}

/// Records a reviewer's disposition of an open finding. Dismissed findings are left out of
/// `inference.findings.v1`. The review is cleared if the cases involved change.
#[utoipa::path(post, path = "/v1/findings/{id}/review", request_body = Review,
    responses((status = 200, body = Finding), (status = 400), (status = 404), (status = 409)))]
async fn review_finding(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(b): Json<Review>,
) -> ApiResult<Json<Finding>> {
    let actor = user(&headers)?;
    let disposition = one_of(
        "disposition",
        b.disposition.as_deref(),
        &["confirmed", "dismissed"],
    )?;
    let note = b.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    if disposition == Some("dismissed") && note.is_none() {
        return Err(ApiError::BadRequest("a note is required to dismiss".into()));
    }
    let mut tx = st.engine.db.begin().await?;
    let f = db::lock_finding(&mut tx, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if f.status != "open" {
        return Err(ApiError::Conflict(format!("finding is {}", f.status)));
    }
    let f = db::review_finding(&mut tx, id, disposition, &actor, note).await?;
    db::touch_findings(&mut tx, f.case_id).await?;
    tx.commit().await?;
    tracing::info!(finding_id = %id, review = ?disposition, by = %actor, "finding reviewed");
    st.engine.spawn_write(f.case_id);
    Ok(Json(f))
}

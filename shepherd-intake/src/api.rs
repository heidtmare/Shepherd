use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shepherd_common::events::{
    EXTRACTION_REQUESTED, Envelope, ExtractionRequested, RECOGNITION_REQUESTED,
    RecognitionRequested,
};
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa::{IntoParams, OpenApi, ToSchema};
use uuid::Uuid;

use crate::{AppState, SERVICE, storage::StorageError};

const INDEX_HTML: &str = include_str!("../static/index.html");
const PDF: &str = "application/pdf";

#[derive(OpenApi)]
#[openapi(
    info(title = "shepherd-intake"),
    paths(upload, reprocess, recognize, form_types)
)]
struct ApiDoc;

pub fn router(state: AppState) -> Router {
    let x_request_id = header::HeaderName::from_static("x-request-id");
    Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/ui/config.json", get(ui_config))
        .route("/v1/intake", post(upload))
        .route("/v1/intake/{id}/reprocess", post(reprocess))
        .route("/v1/intake/{id}/recognize", post(recognize))
        .route("/v1/form-types", get(form_types))
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
    #[error("{0}")]
    UnsupportedMediaType(String),
    #[error("not found")]
    NotFound,
    /// Non-success response from storage, passed through.
    #[error("upstream {0}")]
    Upstream(StatusCode, String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<StorageError> for ApiError {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::NotFound => ApiError::NotFound,
            StorageError::Status { status, body } => ApiError::Upstream(status, body),
            StorageError::Http(e) => ApiError::Internal(e.into()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::Upstream(status, body) => {
                (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
            }
            ApiError::BadRequest(m) => (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "bad_request", "message": m})),
            )
                .into_response(),
            ApiError::UnsupportedMediaType(m) => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                Json(json!({"error": "unsupported_media_type", "message": m})),
            )
                .into_response(),
            ApiError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "not_found", "message": "not found"})),
            )
                .into_response(),
            ApiError::Internal(e) => {
                tracing::error!(error = format!("{e:#}"), "request failed");
                (
                    StatusCode::BAD_GATEWAY,
                    Json(
                        json!({"error": "upstream_unavailable", "message": "storage unavailable"}),
                    ),
                )
                    .into_response()
            }
        }
    }
}

#[derive(Serialize, ToSchema)]
struct UiConfig {
    storage_url: String,
    search_url: String,
    stream_url: String,
    workbench_url: String,
    recognition_enabled: bool,
}

async fn ui_config(State(st): State<AppState>) -> Json<UiConfig> {
    Json(UiConfig {
        storage_url: st.cfg.public_storage_url.clone(),
        search_url: st.cfg.public_search_url.clone(),
        stream_url: st.cfg.public_stream_url.clone(),
        workbench_url: st.cfg.public_workbench_url.clone(),
        recognition_enabled: !st.form_types.is_empty(),
    })
}

fn events_url(st: &AppState, id: Uuid) -> String {
    format!(
        "{}/v1/events?document_id={id}",
        st.cfg.public_stream_url.trim_end_matches('/')
    )
}

#[derive(Deserialize, IntoParams)]
struct UploadParams {
    filename: Option<String>,
    /// Form type or template id (see `/v1/form-types`); skips automatic classification. A form
    /// type with several editions is matched among its editions.
    form_type: Option<String>,
    /// Benefit case receipt number (`AAA` + 10 digits). Links the document to that case, or opens
    /// the case under this number when the document is a benefit form.
    receipt_number: Option<String>,
    /// Date the filing was received, `YYYY-MM-DD`; defaults to the upload date.
    received_date: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct Accepted {
    document_id: Uuid,
    artifact_id: Uuid,
    sha256: String,
    size_bytes: i64,
    /// SSE feed of processing events for this document.
    events_url: String,
}

/// Accept a PDF for processing. The body is the raw file and is streamed through to storage;
/// extraction starts from the resulting `document.created` event.
#[utoipa::path(
    post, path = "/v1/intake", params(UploadParams),
    request_body(content = Vec<u8>, content_type = "application/pdf"),
    responses((status = 202, body = Accepted), (status = 413), (status = 415), (status = 502))
)]
async fn upload(
    State(st): State<AppState>,
    Query(p): Query<UploadParams>,
    headers: HeaderMap,
    body: Body,
) -> Result<(StatusCode, Json<Accepted>), ApiError> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !ct.trim().to_ascii_lowercase().starts_with(PDF) {
        return Err(ApiError::UnsupportedMediaType(format!(
            "content-type must be {PDF}"
        )));
    }
    let actor = headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("intake-upload")
        .to_owned();
    let mut req = st
        .storage
        .http
        .post(format!("{}/v1/documents", st.storage.base_url))
        .header(header::CONTENT_TYPE, PDF)
        .header("x-actor", actor)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()));
    if let Some(f) = &p.filename {
        req = req.query(&[("filename", f)]);
    }
    let mut labels = serde_json::Map::new();
    if let Some(ft) = p.form_type.as_deref().filter(|s| !s.is_empty()) {
        if !crate::FormType::known(&st.form_types, ft) {
            return Err(ApiError::BadRequest(format!("unknown form_type {ft:?}")));
        }
        labels.insert("form_type".into(), json!(ft));
    }
    if let Some(rn) = p.receipt_number.as_deref().filter(|s| !s.is_empty()) {
        let rn: String = rn
            .chars()
            .filter(|c| !matches!(c, ' ' | '-'))
            .collect::<String>()
            .to_ascii_uppercase();
        let valid = rn.len() == 13
            && rn[..3].chars().all(|c| c.is_ascii_uppercase())
            && rn[3..].chars().all(|c| c.is_ascii_digit());
        if !valid {
            return Err(ApiError::BadRequest(
                "receipt_number must be 3 letters and 10 digits".into(),
            ));
        }
        labels.insert("receipt_number".into(), json!(rn));
    }
    if let Some(d) = p.received_date.as_deref().filter(|s| !s.is_empty()) {
        let valid = d.len() == 10
            && d.char_indices().all(|(i, c)| {
                if i == 4 || i == 7 {
                    c == '-'
                } else {
                    c.is_ascii_digit()
                }
            });
        if !valid {
            return Err(ApiError::BadRequest(
                "received_date must be YYYY-MM-DD".into(),
            ));
        }
        labels.insert("received_date".into(), json!(d));
    }
    if !labels.is_empty() {
        req = req.query(&[("labels", Value::Object(labels).to_string())]);
    }
    let resp = req.send().await.map_err(anyhow::Error::from)?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(ApiError::Upstream(status, text));
    }
    let created: Value = resp.json().await.map_err(anyhow::Error::from)?;
    let parse_id = |k: &str| created[k].as_str().and_then(|s| s.parse::<Uuid>().ok());
    let (Some(document_id), Some(artifact_id)) = (parse_id("document_id"), parse_id("artifact_id"))
    else {
        return Err(anyhow::anyhow!("unexpected storage response: {created}").into());
    };
    metrics::counter!("intake_uploads_total").increment(1);
    Ok((
        StatusCode::ACCEPTED,
        Json(Accepted {
            document_id,
            artifact_id,
            sha256: created["sha256"].as_str().unwrap_or_default().to_owned(),
            size_bytes: created["size_bytes"].as_i64().unwrap_or_default(),
            events_url: events_url(&st, document_id),
        }),
    ))
}

#[derive(Serialize, ToSchema)]
struct Requested {
    document_id: Uuid,
    events_url: String,
}

/// Queue re-extraction of the current source artifact. Produces a new IR version.
#[utoipa::path(post, path = "/v1/intake/{id}/reprocess", responses((status = 202, body = Requested), (status = 404)))]
async fn reprocess(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<(StatusCode, Json<Requested>), ApiError> {
    st.storage.document(id).await?;
    let ev = Envelope::new(
        EXTRACTION_REQUESTED,
        SERVICE,
        id,
        ExtractionRequested {
            document_id: id,
            reason: "manual".into(),
        },
    );
    let payload = serde_json::to_vec(&ev).map_err(anyhow::Error::from)?;
    st.js
        .publish(ev.nats_subject(), payload.into())
        .await
        .map_err(anyhow::Error::from)?
        .await
        .map_err(anyhow::Error::from)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(Requested {
            document_id: id,
            events_url: events_url(&st, id),
        }),
    ))
}

#[derive(Deserialize, ToSchema)]
struct RecognizeBody {
    /// Form type or template id; omit to classify automatically.
    form_type: Option<String>,
}

/// Queue form recognition of the current source artifact. Produces a new `intake.form.v1` version.
#[utoipa::path(post, path = "/v1/intake/{id}/recognize", request_body = RecognizeBody,
    responses((status = 202, body = Requested), (status = 400), (status = 404)))]
async fn recognize(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<RecognizeBody>>,
) -> Result<(StatusCode, Json<Requested>), ApiError> {
    if st.form_types.is_empty() {
        return Err(ApiError::BadRequest("form recognition is disabled".into()));
    }
    let form_type = body.and_then(|b| b.0.form_type).filter(|s| !s.is_empty());
    if let Some(ft) = &form_type
        && !crate::FormType::known(&st.form_types, ft)
    {
        return Err(ApiError::BadRequest(format!("unknown form_type {ft:?}")));
    }
    st.storage.document(id).await?;
    let ev = Envelope::new(
        RECOGNITION_REQUESTED,
        SERVICE,
        id,
        RecognitionRequested {
            document_id: id,
            form_type,
            reason: "manual".into(),
        },
    );
    let payload = serde_json::to_vec(&ev).map_err(anyhow::Error::from)?;
    st.js
        .publish(ev.nats_subject(), payload.into())
        .await
        .map_err(anyhow::Error::from)?
        .await
        .map_err(anyhow::Error::from)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(Requested {
            document_id: id,
            events_url: events_url(&st, id),
        }),
    ))
}

/// Form templates this service can recognize.
#[utoipa::path(get, path = "/v1/form-types", responses((status = 200, body = Vec<crate::FormType>)))]
async fn form_types(State(st): State<AppState>) -> Json<Vec<crate::FormType>> {
    Json(st.form_types.as_ref().clone())
}

async fn readyz(State(st): State<AppState>) -> (StatusCode, Json<Value>) {
    let nats = st.nats.connection_state() == async_nats::connection::State::Connected;
    let storage = st.storage.ready().await;
    let status = if nats && storage {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(json!({ "nats": nats, "storage": storage })))
}

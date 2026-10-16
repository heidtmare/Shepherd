use std::collections::{BTreeMap, HashMap};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shepherd_common::{
    events::{Envelope, RECEIPTING_COMPLETED, ReceiptingCompleted},
    storage_client::StorageError,
};
use shepherd_forms::{
    recognize::{FieldResult, FieldStats, FormMatch},
    template::FormTemplate,
};
use time::OffsetDateTime;
use tower_http::{
    compression::CompressionLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa::{IntoParams, OpenApi, ToSchema};
use uuid::Uuid;

use crate::{
    AppState, PRODUCER, SERVICE, db,
    pages::{PageInfo, Pages},
    receipt::{self, Mode, Receipt, Rejection, Submission},
};

const INDEX_HTML: &str = include_str!("../static/index.html");

#[derive(OpenApi)]
#[openapi(
    info(title = "shepherd-workbench"),
    paths(
        list_templates,
        get_template,
        list_tasks,
        create_task,
        get_task,
        update_task,
        claim,
        release,
        cancel,
        save_draft,
        submit,
        recognize,
        history,
        recent_documents,
        document_tasks,
        document_pages,
        page_image
    )
)]
struct ApiDoc;

pub fn router(state: AppState) -> Router {
    let x_request_id = header::HeaderName::from_static("x-request-id");
    Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/v1/templates", get(list_templates))
        .route("/v1/templates/{id}", get(get_template))
        .route("/v1/tasks", get(list_tasks).post(create_task))
        .route("/v1/tasks/{id}", get(get_task).patch(update_task))
        .route("/v1/tasks/{id}/claim", post(claim))
        .route("/v1/tasks/{id}/release", post(release))
        .route("/v1/tasks/{id}/cancel", post(cancel))
        .route("/v1/tasks/{id}/draft", axum::routing::put(save_draft))
        .route("/v1/tasks/{id}/submit", post(submit))
        .route("/v1/tasks/{id}/recognize", post(recognize))
        .route("/v1/tasks/{id}/history", get(history))
        .route("/v1/documents/recent", get(recent_documents))
        .route("/v1/documents/{id}/tasks", get(document_tasks))
        .route("/v1/documents/{id}/pages", get(document_pages))
        .route("/v1/documents/{id}/pages/{n}", get(page_image))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route(
            "/metrics",
            get(|State(st): State<AppState>| async move { st.metrics.render() }),
        )
        .route(
            "/openapi.json",
            get(|| async {
                let mut doc = ApiDoc::openapi();
                doc.merge(crate::case_api::openapi());
                doc.merge(crate::workload_api::openapi());
                Json(doc)
            }),
        )
        .merge(crate::case_api::routes())
        .merge(crate::workload_api::routes())
        .merge(crate::correspondence::routes())
        .merge(crate::findings::routes())
        .layer(middleware::from_fn(shepherd_common::http::track_metrics))
        .layer(CompressionLayer::new())
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
    Forbidden(String),
    #[error("conflict")]
    Conflict(Value),
    #[error("rejected")]
    Rejected(Rejection),
    #[error(transparent)]
    Storage(StorageError),
    /// A service other than storage is unavailable.
    #[error("{0}")]
    Upstream(String),
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
        let body = |code: &str, message: String| json!({ "error": code, "message": message });
        match self {
            ApiError::BadRequest(m) => {
                (StatusCode::BAD_REQUEST, Json(body("bad_request", m))).into_response()
            }
            ApiError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(body("not_found", "not found".into())),
            )
                .into_response(),
            ApiError::Forbidden(m) => {
                (StatusCode::FORBIDDEN, Json(body("forbidden", m))).into_response()
            }
            ApiError::Conflict(v) => (StatusCode::CONFLICT, Json(v)).into_response(),
            ApiError::Rejected(r) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": "rejected", "message": r.message, "fields": r.fields })),
            )
                .into_response(),
            ApiError::Storage(e) => {
                tracing::error!(error = %e, "storage call failed");
                (
                    StatusCode::BAD_GATEWAY,
                    Json(body("upstream", "storage unavailable".into())),
                )
                    .into_response()
            }
            ApiError::Upstream(m) => {
                tracing::warn!(error = %m, "upstream call failed");
                (StatusCode::BAD_GATEWAY, Json(body("upstream", m))).into_response()
            }
            ApiError::Internal(e) => {
                tracing::error!(error = format!("{e:#}"), "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(body("internal", "internal error".into())),
                )
                    .into_response()
            }
        }
    }
}

pub(crate) type ApiResult<T> = Result<T, ApiError>;

/// Reviewer identity from `X-User`. No authentication: deploy behind an auth proxy that sets it.
/// Requesting user, required to hold the manager role (`X-Roles`).
pub(crate) fn manager(headers: &HeaderMap) -> ApiResult<String> {
    let u = user(headers)?;
    let roles = headers.get("x-roles").and_then(|v| v.to_str().ok());
    if crate::workload::has_role(roles, crate::workload::MANAGER_ROLE) {
        Ok(u)
    } else {
        Err(ApiError::Forbidden("manager role required".into()))
    }
}

pub(crate) fn user(headers: &HeaderMap) -> ApiResult<String> {
    headers
        .get("x-user")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|u| !u.is_empty() && u.len() <= 64)
        .map(str::to_owned)
        .ok_or_else(|| ApiError::BadRequest("X-User header required".into()))
}

#[derive(Serialize, ToSchema)]
struct TemplateSummary {
    /// Template (edition) id.
    id: String,
    /// Form type shared by the editions.
    form_type: String,
    aliases: Vec<String>,
    form_number: String,
    title: String,
    edition: Option<String>,
    pages: usize,
    fields: usize,
}

#[utoipa::path(get, path = "/v1/templates", responses((status = 200, body = Vec<TemplateSummary>)))]
async fn list_templates(State(st): State<AppState>) -> Json<Vec<TemplateSummary>> {
    Json(
        st.templates
            .templates
            .iter()
            .map(|t| TemplateSummary {
                id: t.id.clone(),
                form_type: t.form_type().to_owned(),
                aliases: t.aliases.clone(),
                form_number: t.form_number.clone(),
                title: t.title.clone(),
                edition: t.edition.clone(),
                pages: t.pages.len(),
                fields: t.fields.len(),
            })
            .collect(),
    )
}

#[utoipa::path(get, path = "/v1/templates/{id}", responses((status = 200, body = Object), (status = 404)))]
async fn get_template(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<FormTemplate>> {
    st.templates
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or(ApiError::NotFound)
}

#[derive(Deserialize, IntoParams)]
struct ListParams {
    /// Comma-separated: open, in_progress, completed, cancelled. Default: open,in_progress.
    status: Option<String>,
    form_type: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Serialize, ToSchema)]
struct TaskList {
    items: Vec<db::Task>,
    counts: Vec<db::StatusCount>,
}

#[utoipa::path(get, path = "/v1/tasks", params(ListParams), responses((status = 200, body = TaskList)))]
async fn list_tasks(
    State(st): State<AppState>,
    Query(p): Query<ListParams>,
) -> ApiResult<Json<TaskList>> {
    let statuses: Vec<String> = match p.status.as_deref() {
        None => vec!["open".into(), "in_progress".into()],
        Some("all") | Some("") => vec![],
        Some(s) => s.split(',').map(|x| x.trim().to_owned()).collect(),
    };
    for s in &statuses {
        if !matches!(
            s.as_str(),
            "open" | "in_progress" | "completed" | "cancelled"
        ) {
            return Err(ApiError::BadRequest(format!("unknown status {s:?}")));
        }
    }
    let items = db::list(
        &st.db,
        &statuses,
        p.form_type.as_deref().filter(|s| !s.is_empty()),
        p.limit.unwrap_or(100).clamp(1, 500),
        p.offset.unwrap_or(0).max(0),
    )
    .await?;
    Ok(Json(TaskList {
        items,
        counts: db::counts(&st.db).await?,
    }))
}

/// The recognition artifact subset the workbench needs.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Recognition {
    pub artifact_id: Uuid,
    #[schema(value_type = Option<Object>)]
    pub form: Option<FormMatch>,
    pub page_count: u32,
    #[schema(value_type = Object)]
    pub stats: FieldStats,
    #[schema(value_type = Vec<Object>)]
    pub fields: Vec<FieldResult>,
}

#[derive(Deserialize)]
struct FormArtifact {
    form: Option<FormMatch>,
    page_count: u32,
    stats: FieldStats,
    fields: Vec<FieldResult>,
}

pub(crate) async fn load_recognition(st: &AppState, artifact_id: Uuid) -> ApiResult<Recognition> {
    let bytes = st.storage.content(artifact_id).await?;
    let a: FormArtifact = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("form artifact {artifact_id}: {e}"))?;
    Ok(Recognition {
        artifact_id,
        form: a.form,
        page_count: a.page_count,
        stats: a.stats,
        fields: a.fields,
    })
}

pub(crate) async fn source_pdf(st: &AppState, source_artifact: Uuid) -> ApiResult<bytes::Bytes> {
    if let Some(b) = st.pages.cached_pdf(source_artifact) {
        return Ok(b);
    }
    let b = st.storage.content(source_artifact).await?;
    st.pages.put_pdf(source_artifact, b.clone());
    Ok(b)
}

#[derive(Serialize, ToSchema)]
struct TaskDetail {
    task: db::Task,
    #[schema(value_type = Option<Object>)]
    template: Option<FormTemplate>,
    recognition: Option<Recognition>,
    pages: Vec<PageInfo>,
    draft: Option<db::Draft>,
    #[schema(value_type = Option<Object>)]
    receipt: Option<Value>,
}

#[utoipa::path(get, path = "/v1/tasks/{id}", responses((status = 200, body = TaskDetail), (status = 404)))]
async fn get_task(State(st): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<TaskDetail>> {
    let task = db::get(&st.db, id).await?.ok_or(ApiError::NotFound)?;
    let recognition = match task.form_artifact_id {
        Some(a) => Some(load_recognition(&st, a).await?),
        None => None,
    };
    let template = task
        .form_type
        .as_deref()
        .and_then(|f| st.templates.get(f))
        .cloned();
    let pages = Pages::info(source_pdf(&st, task.source_artifact_id).await?).await?;
    let receipt = match task.receipt_artifact_id {
        Some(r) => Some(
            serde_json::from_slice(&st.storage.content(r).await?).map_err(anyhow::Error::from)?,
        ),
        None => None,
    };
    let draft = db::draft(&st.db, id).await?;
    Ok(Json(TaskDetail {
        task,
        template,
        recognition,
        pages,
        draft,
        receipt,
    }))
}

#[derive(Deserialize, ToSchema)]
struct CreateTask {
    document_id: Uuid,
    /// Template id, or a form type (its newest edition); defaults to the latest recognition's match.
    form_type: Option<String>,
}

/// Open a manual receipting task for any stored document.
#[utoipa::path(post, path = "/v1/tasks", request_body = CreateTask,
    responses((status = 201, body = db::Task), (status = 404), (status = 409)))]
async fn create_task(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<CreateTask>,
) -> ApiResult<(StatusCode, Json<db::Task>)> {
    let actor = user(&headers)?;
    let chosen = match b.form_type.as_deref() {
        Some(ft) => Some(
            st.templates
                .resolve(ft)
                .ok_or_else(|| ApiError::BadRequest(format!("unknown form_type {ft:?}")))?
                .id
                .clone(),
        ),
        None => None,
    };
    if let Some(t) = db::active_for_document(&st.db, b.document_id).await? {
        return Err(ApiError::Conflict(
            json!({ "error": "conflict", "message": "document already has an active task", "task_id": t.id }),
        ));
    }
    let doc = st.storage.document(b.document_id).await?;
    let source = doc
        .source()
        .ok_or_else(|| ApiError::BadRequest("document has no source PDF".into()))?
        .clone();
    let latest_form = doc
        .artifacts
        .iter()
        .filter(|a| a.schema == "intake.form.v1")
        .max_by_key(|a| a.version)
        .cloned();
    let recognition = match &latest_form {
        Some(a) => Some(load_recognition(&st, a.id).await?),
        None => None,
    };
    let form_type = chosen.or_else(|| {
        recognition
            .as_ref()
            .and_then(|r| r.form.as_ref())
            .map(|f| f.template_id.clone())
    });
    let total = form_type
        .as_deref()
        .and_then(|f| st.templates.get(f))
        .map_or(0, |t| t.fields.len());
    let flagged = recognition
        .as_ref()
        .map_or(0, |r| r.stats.requires_receipting);
    let mut tx = st.db.begin().await?;
    let (task, _) = db::upsert_active(
        &mut tx,
        &db::NewTask {
            document_id: b.document_id,
            filename: doc.source_filename.as_deref(),
            form_type: form_type.as_deref(),
            kind: "manual",
            source_artifact_id: source.id,
            form_artifact_id: latest_form.map(|a| a.id),
            flagged_fields: flagged as i32,
            total_fields: total as i32,
        },
    )
    .await?;
    db::log(
        &mut tx,
        task.id,
        Some(&actor),
        "created",
        json!({ "kind": "manual", "form_type": form_type }),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(task)))
}

#[derive(Deserialize, ToSchema)]
struct UpdateTask {
    /// Template id, or a form type (its newest edition).
    form_type: String,
}

/// Set the form type (for documents recognition could not classify).
#[utoipa::path(patch, path = "/v1/tasks/{id}", request_body = UpdateTask, responses((status = 200, body = db::Task)))]
async fn update_task(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(b): Json<UpdateTask>,
) -> ApiResult<Json<db::Task>> {
    let actor = user(&headers)?;
    let template = st
        .templates
        .resolve(&b.form_type)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown form_type {:?}", b.form_type)))?;
    let mut tx = st.db.begin().await?;
    let task = db::lock(&mut tx, id).await?.ok_or(ApiError::NotFound)?;
    ensure_active(&task)?;
    let task = db::set_form_type(&mut tx, id, &template.id).await?;
    db::log(
        &mut tx,
        id,
        Some(&actor),
        "form_type_set",
        json!({ "form_type": template.id }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(task))
}

fn ensure_active(t: &db::Task) -> ApiResult<()> {
    if matches!(t.status.as_str(), "open" | "in_progress") {
        Ok(())
    } else {
        Err(ApiError::Conflict(
            json!({ "error": "conflict", "message": format!("task is {}", t.status) }),
        ))
    }
}

fn claim_is_stale(t: &db::Task, ttl_minutes: i64) -> bool {
    t.claimed_at
        .is_none_or(|c| OffsetDateTime::now_utc() - c > time::Duration::minutes(ttl_minutes))
}

/// Fails unless `user` may work on the task (unclaimed, own claim, or stale claim).
fn ensure_can_work(t: &db::Task, user: &str, ttl: i64) -> ApiResult<()> {
    ensure_active(t)?;
    match &t.assignee {
        Some(a) if a != user && !claim_is_stale(t, ttl) => Err(ApiError::Conflict(json!({
            "error": "conflict", "message": format!("claimed by {a}"), "assignee": a
        }))),
        _ => Ok(()),
    }
}

#[utoipa::path(post, path = "/v1/tasks/{id}/claim", responses((status = 200, body = db::Task), (status = 409)))]
async fn claim(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> ApiResult<Json<db::Task>> {
    let actor = user(&headers)?;
    let mut tx = st.db.begin().await?;
    let task = db::lock(&mut tx, id).await?.ok_or(ApiError::NotFound)?;
    ensure_can_work(&task, &actor, st.cfg.claim_ttl_minutes)?;
    let task = db::set_claim(&mut tx, id, Some(&actor)).await?;
    db::log(&mut tx, id, Some(&actor), "claimed", json!({})).await?;
    tx.commit().await?;
    Ok(Json(task))
}

#[utoipa::path(post, path = "/v1/tasks/{id}/release", responses((status = 200, body = db::Task), (status = 409)))]
async fn release(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> ApiResult<Json<db::Task>> {
    let actor = user(&headers)?;
    let mut tx = st.db.begin().await?;
    let task = db::lock(&mut tx, id).await?.ok_or(ApiError::NotFound)?;
    ensure_can_work(&task, &actor, st.cfg.claim_ttl_minutes)?;
    let task = db::set_claim(&mut tx, id, None).await?;
    db::log(&mut tx, id, Some(&actor), "released", json!({})).await?;
    tx.commit().await?;
    Ok(Json(task))
}

#[utoipa::path(post, path = "/v1/tasks/{id}/cancel", responses((status = 200, body = db::Task), (status = 409)))]
async fn cancel(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> ApiResult<Json<db::Task>> {
    let actor = user(&headers)?;
    let mut tx = st.db.begin().await?;
    let task = db::lock(&mut tx, id).await?.ok_or(ApiError::NotFound)?;
    ensure_can_work(&task, &actor, st.cfg.claim_ttl_minutes)?;
    let task = db::cancel(&mut tx, id).await?;
    db::log(&mut tx, id, Some(&actor), "cancelled", json!({})).await?;
    tx.commit().await?;
    Ok(Json(task))
}

#[derive(Deserialize, ToSchema)]
struct Work {
    mode: Mode,
    /// Field id → value; `null` = confirmed blank. Omitted fields keep machine values (review mode).
    #[schema(value_type = Object)]
    values: BTreeMap<String, Option<String>>,
}

#[utoipa::path(put, path = "/v1/tasks/{id}/draft", request_body = Work, responses((status = 200, body = db::Draft)))]
async fn save_draft(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(w): Json<Work>,
) -> ApiResult<Json<db::Draft>> {
    let actor = user(&headers)?;
    reject_amend(w.mode)?;
    let task = db::get(&st.db, id).await?.ok_or(ApiError::NotFound)?;
    ensure_can_work(&task, &actor, st.cfg.claim_ttl_minutes)?;
    let values = serde_json::to_value(&w.values).map_err(anyhow::Error::from)?;
    Ok(Json(
        db::save_draft(&st.db, id, w.mode.as_str(), &values, &actor).await?,
    ))
}

fn reject_amend(m: Mode) -> ApiResult<()> {
    if m == Mode::Amend {
        return Err(ApiError::BadRequest(
            "amend stored receipts through the case (POST /v1/cases/{id}/documents/{doc}/amend)"
                .into(),
        ));
    }
    Ok(())
}

#[derive(Serialize, ToSchema)]
struct Submitted {
    task: db::Task,
    receipt_artifact_id: Uuid,
    version: i32,
    stats: receipt::ReceiptStats,
}

/// Validate and store the receipted values as `receipting.form.v1`, completing the task.
#[utoipa::path(post, path = "/v1/tasks/{id}/submit", request_body = Work,
    responses((status = 200, body = Submitted), (status = 409), (status = 422, body = Rejection)))]
async fn submit(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(w): Json<Work>,
) -> ApiResult<Json<Submitted>> {
    let actor = user(&headers)?;
    reject_amend(w.mode)?;
    let task = db::get(&st.db, id).await?.ok_or(ApiError::NotFound)?;
    ensure_can_work(&task, &actor, st.cfg.claim_ttl_minutes)?;
    // Tasks record the template (edition) id.
    let template_ref = task
        .form_type
        .clone()
        .ok_or_else(|| ApiError::BadRequest("choose a form type first".into()))?;
    let template = st
        .templates
        .get(&template_ref)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown form_type {template_ref:?}")))?;
    let form_type = template.form_type().to_owned();

    let recognition = match task.form_artifact_id {
        Some(a) => Some(load_recognition(&st, a).await?),
        None => None,
    };
    // Machine values only apply when recognition used the same template (edition).
    let machine: HashMap<String, FieldResult> = recognition
        .as_ref()
        .filter(|r| {
            r.form
                .as_ref()
                .and_then(|f| st.templates.get(&f.template_id))
                .is_some_and(|t| t.id == template.id)
        })
        .map(|r| r.fields.iter().map(|f| (f.id.clone(), f.clone())).collect())
        .unwrap_or_default();
    if w.mode == Mode::Review && machine.is_empty() {
        return Err(ApiError::BadRequest(
            "no recognition for this form type; use full receipting".into(),
        ));
    }
    let (fields, stats) = receipt::resolve(&Submission {
        template,
        machine: &machine,
        mode: w.mode,
        values: &w.values,
    })
    .map_err(ApiError::Rejected)?;

    let receipt = Receipt {
        schema: receipt::SCHEMA.into(),
        document_id: task.document_id,
        task_id: task.id,
        form_type: form_type.clone(),
        template_id: Some(template.id.clone()),
        form_number: template.form_number.clone(),
        edition: template.edition.clone(),
        mode: w.mode,
        receipted_by: actor.clone(),
        receipted_at: OffsetDateTime::now_utc(),
        source_artifact_id: task.source_artifact_id,
        form_artifact_id: task.form_artifact_id,
        values: fields
            .iter()
            .filter_map(|f| f.value.clone().map(|v| (f.id.clone(), v)))
            .collect(),
        fields,
        stats: stats.clone(),
        amends: None,
        amendment_reason: None,
    };
    let body = serde_json::to_vec(&receipt).map_err(anyhow::Error::from)?;
    let parent = task.form_artifact_id.unwrap_or(task.source_artifact_id);
    let art = st
        .storage
        .create_artifact_as(
            task.document_id,
            receipt::SCHEMA,
            PRODUCER,
            parent,
            &actor,
            body,
        )
        .await?;

    let mut tx = st.db.begin().await?;
    let locked = db::lock(&mut tx, id).await?.ok_or(ApiError::NotFound)?;
    ensure_can_work(&locked, &actor, st.cfg.claim_ttl_minutes)?;
    let task = db::complete(&mut tx, id, art.id).await?;
    db::delete_draft(&mut tx, id).await?;
    db::log(
        &mut tx,
        id,
        Some(&actor),
        "submitted",
        json!({ "mode": w.mode, "receipt_artifact_id": art.id, "changed": stats.changed }),
    )
    .await?;
    let receipted = crate::case_api::on_receipt(&st, &mut tx, &receipt, art.id, &actor).await?;
    tx.commit().await?;
    if let Some(rc) = receipted {
        rc.after_commit(&st).await;
    }
    metrics::counter!("receipts_total", "mode" => w.mode.as_str()).increment(1);

    let ev = Envelope::new(
        RECEIPTING_COMPLETED,
        SERVICE,
        task.document_id,
        ReceiptingCompleted {
            document_id: task.document_id,
            task_id: task.id,
            artifact_id: art.id,
            version: art.version,
            form_type,
            mode: w.mode.as_str().into(),
            receipted_by: actor,
            changed_fields: stats.changed,
        },
    );
    if let Err(e) = async {
        let payload = serde_json::to_vec(&ev)?;
        st.js
            .publish(ev.nats_subject(), payload.into())
            .await?
            .await?;
        anyhow::Ok(())
    }
    .await
    {
        tracing::warn!(error = %e, "receipting.completed not published");
    }
    Ok(Json(Submitted {
        task,
        receipt_artifact_id: art.id,
        version: art.version,
        stats,
    }))
}

#[derive(Deserialize, ToSchema)]
struct RecognizeBody {
    form_type: String,
}

/// Re-run recognition through intake with the given form type; the task refreshes when it completes.
#[utoipa::path(post, path = "/v1/tasks/{id}/recognize", request_body = RecognizeBody, responses((status = 202)))]
async fn recognize(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(b): Json<RecognizeBody>,
) -> ApiResult<StatusCode> {
    let actor = user(&headers)?;
    let intake = st
        .cfg
        .intake_url
        .clone()
        .ok_or_else(|| ApiError::BadRequest("INTAKE_URL not configured".into()))?;
    let task = db::get(&st.db, id).await?.ok_or(ApiError::NotFound)?;
    ensure_can_work(&task, &actor, st.cfg.claim_ttl_minutes)?;
    if st.templates.candidates(&b.form_type).is_empty() {
        return Err(ApiError::BadRequest(format!(
            "unknown form_type {:?}",
            b.form_type
        )));
    }
    let r = st
        .storage
        .http
        .post(format!(
            "{}/v1/intake/{}/recognize",
            intake.trim_end_matches('/'),
            task.document_id
        ))
        .json(&json!({ "form_type": b.form_type }))
        .send()
        .await
        .map_err(anyhow::Error::from)?;
    if !r.status().is_success() {
        return Err(ApiError::BadRequest(format!(
            "intake returned {}",
            r.status()
        )));
    }
    let mut tx = st.db.begin().await?;
    db::log(
        &mut tx,
        id,
        Some(&actor),
        "recognition_requested",
        json!({ "form_type": b.form_type }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::ACCEPTED)
}

#[utoipa::path(get, path = "/v1/tasks/{id}/history", responses((status = 200, body = Vec<db::TaskEvent>)))]
async fn history(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<db::TaskEvent>>> {
    Ok(Json(db::history(&st.db, id).await?))
}

#[derive(Serialize, ToSchema)]
struct RecentDocument {
    id: Uuid,
    filename: Option<String>,
    created_at: String,
    #[schema(value_type = Object)]
    labels: Value,
    active_task: Option<Uuid>,
}

/// Newest stored documents with their active task, for starting manual receipting.
#[utoipa::path(get, path = "/v1/documents/recent", responses((status = 200, body = Vec<RecentDocument>)))]
async fn recent_documents(State(st): State<AppState>) -> ApiResult<Json<Vec<RecentDocument>>> {
    let page = st.storage.recent_documents(50).await?;
    let mut out = Vec::new();
    for d in page["items"].as_array().into_iter().flatten() {
        let Some(id) = d["id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else {
            continue;
        };
        out.push(RecentDocument {
            id,
            filename: d["source_filename"].as_str().map(str::to_owned),
            created_at: d["created_at"].as_str().unwrap_or_default().to_owned(),
            labels: d["labels"].clone(),
            active_task: db::active_for_document(&st.db, id).await?.map(|t| t.id),
        });
    }
    Ok(Json(out))
}

#[utoipa::path(get, path = "/v1/documents/{id}/tasks", responses((status = 200, body = Vec<db::Task>)))]
async fn document_tasks(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<db::Task>>> {
    Ok(Json(db::tasks_for_document(&st.db, id).await?))
}

async fn source_of(st: &AppState, document_id: Uuid) -> ApiResult<Uuid> {
    let doc = st.storage.document(document_id).await?;
    Ok(doc.source().ok_or(ApiError::NotFound)?.id)
}

#[utoipa::path(get, path = "/v1/documents/{id}/pages", responses((status = 200, body = Vec<PageInfo>)))]
async fn document_pages(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<PageInfo>>> {
    let source = source_of(&st, id).await?;
    Ok(Json(Pages::info(source_pdf(&st, source).await?).await?))
}

/// Page `n` (1-based) of the source PDF as PNG.
#[utoipa::path(get, path = "/v1/documents/{id}/pages/{n}", responses((status = 200, content_type = "image/png"), (status = 404)))]
async fn page_image(
    State(st): State<AppState>,
    Path((id, n)): Path<(Uuid, u32)>,
) -> ApiResult<Response> {
    if n == 0 {
        return Err(ApiError::NotFound);
    }
    let source = source_of(&st, id).await?;
    let pdf = source_pdf(&st, source).await?;
    let png = st
        .pages
        .png(source, pdf, n)
        .await
        .map_err(|_| ApiError::NotFound)?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "private, max-age=3600"),
        ],
        png.as_ref().clone(),
    )
        .into_response())
}

async fn readyz(State(st): State<AppState>) -> (StatusCode, Json<Value>) {
    let db = sqlx::query("SELECT 1").execute(&st.db).await.is_ok();
    let nats = st.nats.connection_state() == async_nats::connection::State::Connected;
    let storage = st.storage.ready().await;
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

//! Workload HTTP API: workers and skills, assignment rules, bulk reassignment and workload
//! requests. Managing workers, rules and assignments requires the manager role (`X-Roles`).

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{IntoParams, OpenApi, ToSchema};
use uuid::Uuid;

use crate::{
    AppState,
    api::{ApiError, ApiResult, manager, user},
    case::Status,
    case_db,
    receipt::Rejection,
    workload::{
        self, Refusal, RequestKind, RequestStatus, RuleInput, WorkerInput, check_request, one,
    },
    workload_db::{self, Request, Rule, Worker},
    workload_ops::{self, Via},
};

#[derive(OpenApi)]
#[openapi(paths(
    list_workers,
    get_worker,
    put_worker,
    delete_worker,
    list_rules,
    create_rule,
    update_rule,
    delete_rule,
    reassign,
    sweep,
    create_request,
    list_requests,
    approve_request,
    deny_request,
    withdraw_request
))]
struct WorkloadDoc;

pub fn openapi() -> utoipa::openapi::OpenApi {
    WorkloadDoc::openapi()
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/workload/workers", get(list_workers))
        .route(
            "/v1/workload/workers/{name}",
            get(get_worker).put(put_worker).delete(delete_worker),
        )
        .route("/v1/workload/rules", get(list_rules).post(create_rule))
        .route(
            "/v1/workload/rules/{id}",
            put(update_rule).delete(delete_rule),
        )
        .route("/v1/workload/reassign", post(reassign))
        .route("/v1/workload/sweep", post(sweep))
        .route("/v1/workload/requests", get(list_requests))
        .route("/v1/workload/requests/{id}/approve", post(approve_request))
        .route("/v1/workload/requests/{id}/deny", post(deny_request))
        .route(
            "/v1/workload/requests/{id}/withdraw",
            post(withdraw_request),
        )
        .route("/v1/cases/{id}/requests", post(create_request))
}

fn conflict(message: impl Into<String>) -> ApiError {
    ApiError::Conflict(json!({ "error": "conflict", "message": message.into() }))
}

/// Skill problems are 422 on `assignee`; capacity is 409 `over_capacity` (retry with `force`).
pub(crate) fn refused(r: &Refusal, worker: &str, form_type: &str) -> ApiError {
    let message = r.message(worker, form_type);
    match r {
        Refusal::OverCapacity { load, max_cases } => ApiError::Conflict(json!({
            "error": "over_capacity", "message": message, "worker": worker,
            "load": load, "max_cases": max_cases
        })),
        _ => ApiError::Rejected(one("assignee", message)),
    }
}

fn worker_name(s: &str, headers: &HeaderMap) -> ApiResult<String> {
    if s == "me" {
        return user(headers);
    }
    workload::normalize_worker(s)
        .ok_or_else(|| ApiError::BadRequest("names are 1 to 64 characters".into()))
}

async fn load_worker(st: &AppState, name: &str) -> ApiResult<Worker> {
    workload_db::worker(&mut *st.db.acquire().await?, name)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Workers with skills, capacity and current load.
#[utoipa::path(get, path = "/v1/workload/workers", responses((status = 200, body = Vec<Worker>)))]
async fn list_workers(State(st): State<AppState>) -> ApiResult<Json<Vec<Worker>>> {
    Ok(Json(workload_db::workers(&st.db).await?))
}

/// One worker; `me` is the requesting user.
#[utoipa::path(get, path = "/v1/workload/workers/{name}",
    responses((status = 200, body = Worker), (status = 404)))]
async fn get_worker(
    State(st): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Worker>> {
    let name = worker_name(&name, &headers)?;
    Ok(Json(load_worker(&st, &name).await?))
}

/// Register a worker or replace their settings and skills (manager). A skill cannot be removed
/// while the worker holds undecided cases of that form. Refills the worker's workload.
#[utoipa::path(put, path = "/v1/workload/workers/{name}", request_body = WorkerInput,
    responses((status = 200, body = Worker), (status = 403), (status = 409), (status = 422, body = Rejection)))]
async fn put_worker(
    State(st): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(b): Json<WorkerInput>,
) -> ApiResult<Json<Worker>> {
    let actor = manager(&headers)?;
    let name = worker_name(&name, &headers)?;
    let settings = workload::check_worker(&st.benefits, &b, st.cfg.workload_default_max_cases)
        .map_err(ApiError::Rejected)?;
    let mut tx = st.db.begin().await?;
    workload_db::lock_assignments(&mut tx).await?;
    let held = workload_db::held_forms(&mut tx, &name).await?;
    let orphaned: Vec<String> = held
        .iter()
        .filter(|(f, _)| !settings.skills.contains(f))
        .map(|(f, n)| format!("{n} {f}"))
        .collect();
    if !orphaned.is_empty() {
        return Err(ApiError::Conflict(json!({
            "error": "conflict",
            "message": format!("{name} holds undecided cases ({}); reassign them first", orphaned.join(", ")),
        })));
    }
    workload_db::upsert_worker(&mut tx, &name, &settings, &actor).await?;
    tx.commit().await?;
    st.assigner.spawn_rebalance(None, vec![name.clone()]);
    Ok(Json(load_worker(&st, &name).await?))
}

/// Remove a worker (manager). Their undecided cases must be reassigned first; pending requests
/// are cancelled.
#[utoipa::path(delete, path = "/v1/workload/workers/{name}",
    responses((status = 204), (status = 403), (status = 404), (status = 409)))]
async fn delete_worker(
    State(st): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    manager(&headers)?;
    let name = worker_name(&name, &headers)?;
    let mut tx = st.db.begin().await?;
    workload_db::lock_assignments(&mut tx).await?;
    let n = workload_db::load(&mut tx, &name, None).await?;
    if n > 0 {
        return Err(conflict(format!(
            "{name} holds {n} undecided case(s); reassign them first"
        )));
    }
    workload_db::cancel_worker_requests(&mut tx, &name, "worker removed").await?;
    if !workload_db::delete_worker(&mut tx, &name).await? {
        return Err(ApiError::NotFound);
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get, path = "/v1/workload/rules", responses((status = 200, body = Vec<Rule>)))]
async fn list_rules(State(st): State<AppState>) -> ApiResult<Json<Vec<Rule>>> {
    Ok(Json(workload_db::rules(&st.db).await?))
}

/// Create an assignment rule (manager). The unassigned backlog is swept afterwards.
#[utoipa::path(post, path = "/v1/workload/rules", request_body = RuleInput,
    responses((status = 201, body = Rule), (status = 403), (status = 422, body = Rejection)))]
async fn create_rule(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<RuleInput>,
) -> ApiResult<(StatusCode, Json<Rule>)> {
    let actor = manager(&headers)?;
    let r = workload::check_rule(&st.benefits, &b).map_err(ApiError::Rejected)?;
    let rule = workload_db::insert_rule(&mut *st.db.acquire().await?, &r, &actor).await?;
    st.assigner.spawn_sweep();
    Ok((StatusCode::CREATED, Json(rule)))
}

/// Replace an assignment rule (manager).
#[utoipa::path(put, path = "/v1/workload/rules/{id}", request_body = RuleInput,
    responses((status = 200, body = Rule), (status = 403), (status = 404), (status = 422, body = Rejection)))]
async fn update_rule(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(b): Json<RuleInput>,
) -> ApiResult<Json<Rule>> {
    let actor = manager(&headers)?;
    let r = workload::check_rule(&st.benefits, &b).map_err(ApiError::Rejected)?;
    let rule = workload_db::update_rule(&mut *st.db.acquire().await?, id, &r, &actor)
        .await?
        .ok_or(ApiError::NotFound)?;
    st.assigner.spawn_sweep();
    Ok(Json(rule))
}

/// Delete an assignment rule (manager). Existing assignments are kept.
#[utoipa::path(delete, path = "/v1/workload/rules/{id}",
    responses((status = 204), (status = 403), (status = 404)))]
async fn delete_rule(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    manager(&headers)?;
    if workload_db::delete_rule(&mut *st.db.acquire().await?, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

#[derive(Deserialize, ToSchema)]
struct ReassignBody {
    /// 1 to 500 undecided cases.
    case_ids: Vec<Uuid>,
    /// Worker to receive the cases; `null` returns them to the unassigned pool.
    assignee: Option<String>,
    /// Exceed the worker's `max_cases`.
    #[serde(default)]
    force: bool,
}

#[derive(Serialize, ToSchema)]
struct Reassigned {
    /// Cases whose assignee changed.
    changed: Vec<Uuid>,
}

/// Move cases to another worker or back to the pool (manager). All or nothing: any refusal
/// rolls back the batch. Workers the cases are taken from are refilled.
#[utoipa::path(post, path = "/v1/workload/reassign", request_body = ReassignBody,
    responses((status = 200, body = Reassigned), (status = 403), (status = 404), (status = 409), (status = 422, body = Rejection)))]
async fn reassign(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<ReassignBody>,
) -> ApiResult<Json<Reassigned>> {
    let actor = manager(&headers)?;
    let mut ids = b.case_ids.clone();
    ids.sort();
    ids.dedup();
    if ids.is_empty() || ids.len() > 500 {
        return Err(ApiError::Rejected(one("case_ids", "1 to 500 cases")));
    }
    let target =
        match b.assignee.as_deref() {
            Some(a) => Some(workload::normalize_worker(a).ok_or_else(|| {
                ApiError::Rejected(one("assignee", "names are 1 to 64 characters"))
            })?),
            None => None,
        };
    let mut tx = st.db.begin().await?;
    workload_db::lock_assignments(&mut tx).await?;
    let mut changes = Vec::new();
    for id in &ids {
        let c = case_db::lock(&mut tx, *id)
            .await?
            .ok_or(ApiError::NotFound)?;
        if c.status == Status::Decided.as_str() {
            return Err(conflict(format!("{} is decided", c.receipt_number)));
        }
        let ch = match &target {
            Some(t) => workload_ops::assign_checked(&mut tx, &c, t, b.force, &actor, Via::Manager)
                .await?
                .map_err(|r| refused(&r, t, &c.form_type))?,
            None => workload_ops::set(&mut tx, &c, None, Some(&actor), Via::Manager).await?,
        };
        changes.extend(ch);
    }
    tx.commit().await?;
    st.assigner.published(&changes).await;
    let mut previous: Vec<String> = changes.iter().filter_map(|c| c.previous.clone()).collect();
    previous.sort();
    previous.dedup();
    if target.is_none() {
        for ch in &changes {
            st.assigner.spawn_dispatch(ch.case_id);
        }
    }
    st.assigner.spawn_rebalance(None, previous);
    Ok(Json(Reassigned {
        changed: changes.iter().map(|c| c.case_id).collect(),
    }))
}

#[derive(Serialize, ToSchema)]
struct Swept {
    assigned: usize,
}

/// Run automatic assignment of the unassigned backlog now (manager).
#[utoipa::path(post, path = "/v1/workload/sweep",
    responses((status = 200, body = Swept), (status = 403)))]
async fn sweep(State(st): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Swept>> {
    manager(&headers)?;
    Ok(Json(Swept {
        assigned: st.assigner.sweep().await?,
    }))
}

#[derive(Deserialize, ToSchema)]
struct RequestBody {
    kind: RequestKind,
    reason: Option<String>,
}

/// Ask a manager to add the case to, or remove it from, the requesting worker's workload.
#[utoipa::path(post, path = "/v1/cases/{id}/requests", request_body = RequestBody,
    responses((status = 201, body = Request), (status = 404), (status = 409), (status = 422, body = Rejection)))]
async fn create_request(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(b): Json<RequestBody>,
) -> ApiResult<(StatusCode, Json<Request>)> {
    let actor = user(&headers)?;
    let reason = workload::clean_text("reason", b.reason.as_deref()).map_err(ApiError::Rejected)?;
    let mut tx = st.db.begin().await?;
    let c = case_db::lock(&mut tx, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let status = Status::parse(&c.status).expect("status constrained by schema");
    check_request(b.kind, status, c.assignee.as_deref(), &actor).map_err(conflict)?;
    if b.kind == RequestKind::Add {
        let e = workload_ops::eligibility(&mut tx, &c, &actor).await?;
        if let Err(r) = workload::check_assignment(&e, true) {
            return Err(refused(&r, &actor, &c.form_type));
        }
    }
    let Some(rid) = workload_db::insert_request(
        &mut tx,
        &workload_db::NewRequest {
            case_id: id,
            worker: &actor,
            kind: b.kind.as_str(),
            reason: reason.as_deref(),
        },
    )
    .await?
    else {
        let existing = workload_db::pending_request(&mut tx, id, &actor, b.kind.as_str()).await?;
        return Err(ApiError::Conflict(json!({
            "error": "conflict", "message": "you already have a pending request", "request_id": existing
        })));
    };
    case_db::log(
        &mut tx,
        id,
        Some(&actor),
        "workload_requested",
        json!({ "request_id": rid, "kind": b.kind, "reason": reason }),
    )
    .await?;
    let q = workload_db::request(&mut tx, rid)
        .await?
        .expect("inserted request");
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(q)))
}

#[derive(Deserialize, IntoParams)]
struct RequestParams {
    /// Comma-separated statuses, or `all`. Default: pending.
    status: Option<String>,
    /// Worker name, or `me`.
    worker: Option<String>,
    case_id: Option<Uuid>,
    limit: Option<i64>,
}

/// Workload requests: pending oldest first, then resolved newest first.
#[utoipa::path(get, path = "/v1/workload/requests", params(RequestParams),
    responses((status = 200, body = Vec<Request>)))]
async fn list_requests(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(p): Query<RequestParams>,
) -> ApiResult<Json<Vec<Request>>> {
    let statuses: Vec<String> = match p.status.as_deref() {
        None => vec!["pending".into()],
        Some("all") | Some("") => vec![],
        Some(s) => s.split(',').map(|x| x.trim().to_owned()).collect(),
    };
    if let Some(s) = statuses.iter().find(|s| RequestStatus::parse(s).is_none()) {
        return Err(ApiError::BadRequest(format!("unknown status {s:?}")));
    }
    let worker = match p.worker.as_deref().filter(|s| !s.is_empty()) {
        Some(w) => Some(worker_name(w, &headers)?),
        None => None,
    };
    Ok(Json(
        workload_db::requests(
            &st.db,
            &workload_db::RequestFilter {
                statuses: &statuses,
                worker: worker.as_deref(),
                case_id: p.case_id,
                limit: p.limit.unwrap_or(200).clamp(1, 1000),
            },
        )
        .await?,
    ))
}

#[derive(Deserialize, ToSchema, Default)]
struct ResolveBody {
    note: Option<String>,
    /// Approving an addition: exceed the worker's `max_cases`.
    #[serde(default)]
    force: bool,
}

/// Locks a pending request and its case. The caller holds the assignment lock.
async fn pending(conn: &mut sqlx::PgConnection, id: Uuid) -> ApiResult<(Request, case_db::Case)> {
    let q = workload_db::request(conn, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if q.status != RequestStatus::Pending.as_str() {
        return Err(conflict(format!("request is {}", q.status)));
    }
    let c = case_db::lock(conn, q.case_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok((q, c))
}

async fn resolve(
    conn: &mut sqlx::PgConnection,
    q: &Request,
    status: RequestStatus,
    actor: &str,
    note: Option<&str>,
) -> ApiResult<Request> {
    if !workload_db::resolve_request(conn, q.id, status.as_str(), Some(actor), note).await? {
        return Err(conflict("request is no longer pending"));
    }
    case_db::log(
        conn,
        q.case_id,
        Some(actor),
        "workload_request_resolved",
        json!({ "request_id": q.id, "kind": q.kind, "worker": q.worker, "status": status, "note": note }),
    )
    .await?;
    Ok(workload_db::request(conn, q.id)
        .await?
        .expect("resolved request"))
}

/// Approve a request (manager): an addition assigns the case to the worker (skill required;
/// capacity unless `force`), a removal returns it to the pool for rule assignment.
#[utoipa::path(post, path = "/v1/workload/requests/{id}/approve", request_body = ResolveBody,
    responses((status = 200, body = Request), (status = 403), (status = 404), (status = 409), (status = 422, body = Rejection)))]
async fn approve_request(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    body: Option<Json<ResolveBody>>,
) -> ApiResult<Json<Request>> {
    let actor = manager(&headers)?;
    let b = body.map(|b| b.0).unwrap_or_default();
    let note = workload::clean_text("note", b.note.as_deref()).map_err(ApiError::Rejected)?;
    let mut tx = st.db.begin().await?;
    workload_db::lock_assignments(&mut tx).await?;
    let (q, c) = pending(&mut tx, id).await?;
    let kind = RequestKind::parse(&q.kind).expect("kind constrained by schema");
    let status = Status::parse(&c.status).expect("status constrained by schema");
    check_request(kind, status, c.assignee.as_deref(), &q.worker)
        .map_err(|m| conflict(format!("{m} for {}; deny the request instead", q.worker)))?;
    let change = match kind {
        RequestKind::Add => {
            workload_ops::assign_checked(&mut tx, &c, &q.worker, b.force, &actor, Via::Request(id))
                .await?
                .map_err(|r| refused(&r, &q.worker, &c.form_type))?
        }
        RequestKind::Remove => {
            workload_ops::set(&mut tx, &c, None, Some(&actor), Via::Request(id)).await?
        }
    };
    let resolved = resolve(
        &mut tx,
        &q,
        RequestStatus::Approved,
        &actor,
        note.as_deref(),
    )
    .await?;
    tx.commit().await?;
    if let Some(ch) = change {
        st.assigner.published(std::slice::from_ref(&ch)).await;
        let redispatch = (kind == RequestKind::Remove).then_some(ch.case_id);
        st.assigner
            .spawn_rebalance(redispatch, ch.previous.into_iter().collect());
    }
    Ok(Json(resolved))
}

/// Deny a request (manager).
#[utoipa::path(post, path = "/v1/workload/requests/{id}/deny", request_body = ResolveBody,
    responses((status = 200, body = Request), (status = 403), (status = 404), (status = 409)))]
async fn deny_request(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    body: Option<Json<ResolveBody>>,
) -> ApiResult<Json<Request>> {
    let actor = manager(&headers)?;
    let b = body.map(|b| b.0).unwrap_or_default();
    let note = workload::clean_text("note", b.note.as_deref()).map_err(ApiError::Rejected)?;
    let mut tx = st.db.begin().await?;
    let (q, _) = pending(&mut tx, id).await?;
    let resolved = resolve(&mut tx, &q, RequestStatus::Denied, &actor, note.as_deref()).await?;
    tx.commit().await?;
    Ok(Json(resolved))
}

/// Withdraw one's own pending request.
#[utoipa::path(post, path = "/v1/workload/requests/{id}/withdraw",
    responses((status = 200, body = Request), (status = 403), (status = 404), (status = 409)))]
async fn withdraw_request(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> ApiResult<Json<Request>> {
    let actor = user(&headers)?;
    let mut tx = st.db.begin().await?;
    let (q, _) = pending(&mut tx, id).await?;
    if q.worker != actor {
        return Err(ApiError::Forbidden(
            "only the requester can withdraw".into(),
        ));
    }
    let resolved = resolve(&mut tx, &q, RequestStatus::Withdrawn, &actor, None).await?;
    tx.commit().await?;
    Ok(Json(resolved))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_map_to_status() {
        let over = refused(
            &Refusal::OverCapacity {
                load: 3,
                max_cases: 3,
            },
            "ana",
            "uscis.i-765",
        );
        assert!(
            matches!(over, ApiError::Conflict(v) if v["error"] == "over_capacity" && v["load"] == 3)
        );
        let unskilled = refused(&Refusal::Unskilled, "ana", "uscis.i-765");
        assert!(
            matches!(unskilled, ApiError::Rejected(r) if r.fields.get("assignee").is_some_and(|m| m.contains("not skilled")))
        );
    }
}

//! Case HTTP API: worklist, detail, editing, assessment, notices and decisions.
//!
//! Mutations of an assigned case are limited to its assignee; an unassigned case may be edited by
//! anyone, but assessment, notices and decisions require assignment first. Assignment is a
//! manager action or automatic (see `workload_ops`); workers request changes to their workload.

use std::collections::{BTreeMap, HashMap};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shepherd_common::events::{
    CASE_DECIDED, CASE_OPENED, CASE_REOPENED, CaseDecided, CaseOpened, CaseReopened,
    NOTICE_EXTENDED, NOTICE_ISSUED, NOTICE_RESCINDED, NoticeExtended, NoticeIssued,
    NoticeRescinded, RECEIPTING_COMPLETED, ReceiptingCompleted,
};
use shepherd_forms::recognize::FieldResult;
use sqlx::PgConnection;
use time::OffsetDateTime;
use utoipa::{IntoParams, OpenApi, ToSchema};
use uuid::Uuid;

use crate::{
    AppState, PRODUCER,
    api::{ApiError, ApiResult, Recognition, load_recognition, manager, source_pdf, user},
    benefits::{Benefit, Benefits, Ground, normalize_fact},
    case::{
        self, AssessmentInput, ClosureReason, CriterionResult, DECISION_SCHEMA, DecisionContext,
        DecisionInput, ExtensionInput, NOTICE_SCHEMA, NoticeInput, NoticeKind, Outcome, Status,
    },
    case_db::{self, Case, CaseDocument, CaseEvent, Notice},
    case_ops::{self, FormValues},
    db, discovery, findings,
    pages::{PageInfo, Pages},
    receipt::{self, Mode, Receipt, Rejection},
    workload,
    workload_api::refused,
    workload_db,
    workload_ops::{self, Change, Via},
};

#[derive(OpenApi)]
#[openapi(paths(
    get_benefits,
    list_cases,
    create_case,
    get_case,
    update_case,
    assign,
    unassign,
    save_criteria,
    issue_notice,
    record_response,
    extend_notice,
    rescind_notice,
    decide,
    reopen,
    add_document,
    remove_document,
    change_primary,
    amend,
    add_note,
    case_history,
    document_form,
    discovery::search
))]
struct CaseDoc;

pub fn openapi() -> utoipa::openapi::OpenApi {
    CaseDoc::openapi()
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/benefits", get(get_benefits))
        .route("/v1/cases", get(list_cases).post(create_case))
        .route("/v1/cases/search", post(discovery::search))
        .route("/v1/cases/{id}", get(get_case).patch(update_case))
        .route("/v1/cases/{id}/assign", post(assign))
        .route("/v1/cases/{id}/unassign", post(unassign))
        .route("/v1/cases/{id}/criteria", put(save_criteria))
        .route("/v1/cases/{id}/notices", post(issue_notice))
        .route(
            "/v1/cases/{id}/notices/{nid}/response",
            post(record_response),
        )
        .route(
            "/v1/cases/{id}/notices/{nid}/extension",
            post(extend_notice),
        )
        .route("/v1/cases/{id}/notices/{nid}/rescind", post(rescind_notice))
        .route("/v1/cases/{id}/decision", post(decide))
        .route("/v1/cases/{id}/reopen", post(reopen))
        .route("/v1/cases/{id}/documents", post(add_document))
        .route("/v1/cases/{id}/documents/{doc}", delete(remove_document))
        .route("/v1/cases/{id}/documents/{doc}/amend", post(amend))
        .route("/v1/cases/{id}/primary", post(change_primary))
        .route("/v1/cases/{id}/notes", post(add_note))
        .route("/v1/cases/{id}/history", get(case_history))
        .route("/v1/documents/{id}/form", get(document_form))
}

fn conflict(message: impl Into<String>) -> ApiError {
    ApiError::Conflict(json!({ "error": "conflict", "message": message.into() }))
}

fn rejected(field: &str, problem: &str) -> ApiError {
    ApiError::Rejected(Rejection {
        message: problem.into(),
        fields: BTreeMap::from([(field.to_owned(), problem.to_owned())]),
    })
}

fn status(c: &Case) -> Status {
    Status::parse(&c.status).expect("status constrained by schema")
}

pub(crate) fn ensure_undecided(c: &Case) -> ApiResult<()> {
    if status(c) == Status::Decided {
        Err(conflict("case is decided; reopen it first"))
    } else {
        Ok(())
    }
}

/// Editing: unassigned, or assigned to `actor`.
fn ensure_can_edit(c: &Case, actor: &str) -> ApiResult<()> {
    match &c.assignee {
        Some(a) if a != actor => Err(ApiError::Conflict(json!({
            "error": "conflict", "message": format!("assigned to {a}"), "assignee": a
        }))),
        _ => Ok(()),
    }
}

/// Assessment, notices and decisions: assigned to `actor`.
pub(crate) fn ensure_assignee(c: &Case, actor: &str) -> ApiResult<()> {
    match &c.assignee {
        Some(a) if a == actor => Ok(()),
        Some(a) => Err(ApiError::Conflict(json!({
            "error": "conflict", "message": format!("assigned to {a}"), "assignee": a
        }))),
        None => Err(conflict("assign the case to yourself first")),
    }
}

fn benefit<'a>(st: &'a AppState, c: &Case) -> ApiResult<&'a Benefit> {
    st.benefits.get(&c.form_type).ok_or_else(|| {
        anyhow::anyhow!("case {} has unconfigured form type {}", c.id, c.form_type).into()
    })
}

pub(crate) async fn load(st: &AppState, id: Uuid) -> ApiResult<Case> {
    case_db::get(&st.db, id).await?.ok_or(ApiError::NotFound)
}

async fn lock(conn: &mut PgConnection, id: Uuid) -> ApiResult<Case> {
    case_db::lock(conn, id).await?.ok_or(ApiError::NotFound)
}

fn today() -> time::Date {
    OffsetDateTime::now_utc().date()
}

fn overdue(c: &Case) -> bool {
    status(c) == Status::AwaitingResponse
        && c.response_due
            .as_deref()
            .and_then(|d| case::parse_date(d).ok())
            .is_some_and(|d| d < today())
}

async fn primary_values(st: &AppState, c: &Case) -> ApiResult<Option<FormValues>> {
    match case_ops::document(&st.storage, c.primary_document_id).await? {
        Some(doc) => Ok(case_ops::form_values(&st.storage, &doc).await?),
        None => Ok(None),
    }
}

/// Case affected by a receipt.
pub(crate) struct Receipted {
    pub case_id: Uuid,
    /// Assignee removed by a form type change.
    pub unassigned: Option<Change>,
}

impl Receipted {
    /// Re-projects the case and dispatches it (it may be new or unassigned).
    pub(crate) async fn after_commit(self, st: &AppState) {
        st.projector.spawn_sync(self.case_id);
        after_assignment(st, self.unassigned, Some(self.case_id)).await;
    }
}

/// Called in the receipting transaction: logs the receipt on the document's case, refreshes facts
/// from a primary form (following a changed benefit form type, see [`workload_ops::retype`]), or
/// opens a case for an unlinked benefit form.
pub(crate) async fn on_receipt(
    st: &AppState,
    conn: &mut PgConnection,
    r: &Receipt,
    artifact_id: Uuid,
    actor: &str,
) -> ApiResult<Option<Receipted>> {
    let benefit = st.benefits.get(&r.form_type);
    let template = st.templates.get(r.template_id());
    match case_db::for_document(conn, r.document_id).await? {
        Some((c, role)) => {
            // Amendments log their own case event with the reason.
            if r.mode != Mode::Amend {
                case_db::log(
                    conn,
                    c.id,
                    Some(actor),
                    "receipted",
                    json!({ "document_id": r.document_id, "receipt_artifact_id": artifact_id, "mode": r.mode }),
                )
                .await?;
            }
            if role == "primary"
                && status(&c) != Status::Decided
                && let Some(b) = benefit
            {
                let mut c = c;
                let mut unassigned = None;
                if c.form_type != r.form_type {
                    workload_db::lock_assignments(conn).await?;
                    c = case_db::lock(conn, c.id).await?.ok_or(ApiError::NotFound)?;
                    if c.form_type != r.form_type && status(&c) != Status::Decided {
                        let rt = workload_ops::retype(conn, &c, &r.form_type, Some(actor)).await?;
                        case_db::log(
                            conn,
                            c.id,
                            Some(actor),
                            "form_type_changed",
                            json!({ "from": c.form_type, "to": r.form_type, "criteria_cleared": rt.cleared }),
                        )
                        .await?;
                        (c, unassigned) = (rt.case, rt.change);
                    }
                }
                case_ops::refresh_facts(conn, &c, b, template, &r.values).await?;
                return Ok(Some(Receipted {
                    case_id: c.id,
                    unassigned,
                }));
            }
            Ok(Some(Receipted {
                case_id: c.id,
                unassigned: None,
            }))
        }
        None => {
            let Some(b) = benefit else { return Ok(None) };
            let Some(doc) = case_ops::document(&st.storage, r.document_id).await? else {
                return Ok(None);
            };
            let label = case_ops::receipt_label(&doc);
            if let Some(rn) = &label
                && let Some(existing) = case_db::by_receipt(conn, rn).await?
            {
                case_ops::attach(
                    conn,
                    &existing,
                    r.document_id,
                    doc.source_filename.as_deref(),
                    None,
                    Some(actor),
                )
                .await?;
                return Ok(Some(Receipted {
                    case_id: existing.id,
                    unassigned: None,
                }));
            }
            let c = case_ops::open_case(
                conn,
                &st.cfg.receipt_prefix,
                &case_ops::Opening {
                    receipt_number: label.as_deref(),
                    form_type: &b.form_type,
                    document_id: r.document_id,
                    filename: doc.source_filename.as_deref(),
                    received_date: case_ops::received_date(&doc),
                    facts: &b.derive_facts(template, &r.values),
                    actor: Some(actor),
                },
            )
            .await?;
            let (js, ev) = (
                st.js.clone(),
                CaseOpened {
                    case_id: c.id,
                    receipt_number: c.receipt_number.clone(),
                    form_type: c.form_type.clone(),
                    document_id: r.document_id,
                },
            );
            let doc_id = r.document_id;
            tokio::spawn(async move { crate::publish(&js, CASE_OPENED, doc_id, ev).await });
            Ok(Some(Receipted {
                case_id: c.id,
                unassigned: None,
            }))
        }
    }
}

#[utoipa::path(get, path = "/v1/benefits", responses((status = 200, body = Benefits)))]
async fn get_benefits(State(st): State<AppState>) -> Json<Benefits> {
    Json(st.benefits.as_ref().clone())
}

#[derive(Deserialize, IntoParams)]
struct ListParams {
    /// Comma-separated statuses, or `all`. Default: open,in_review,awaiting_response.
    status: Option<String>,
    /// User name, or `me` (the `X-User` header).
    assignee: Option<String>,
    form_type: Option<String>,
    /// Only cases awaiting a response past its due date, earliest due first.
    overdue: Option<bool>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Serialize, ToSchema)]
struct CaseList {
    items: Vec<Case>,
    counts: Vec<case_db::Count>,
    /// Cases awaiting a response past its due date.
    overdue: i64,
    /// Undecided cases assigned to the requesting user (0 without `X-User`).
    mine: i64,
}

pub(crate) fn parse_statuses(s: Option<&str>, default: &[&str]) -> ApiResult<Vec<String>> {
    let v: Vec<String> = match s {
        None => default.iter().map(|s| (*s).to_owned()).collect(),
        Some("all") | Some("") => vec![],
        Some(s) => s.split(',').map(|x| x.trim().to_owned()).collect(),
    };
    for s in &v {
        if Status::parse(s).is_none() {
            return Err(ApiError::BadRequest(format!("unknown status {s:?}")));
        }
    }
    Ok(v)
}

/// Case worklist.
#[utoipa::path(get, path = "/v1/cases", params(ListParams), responses((status = 200, body = CaseList)))]
async fn list_cases(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(p): Query<ListParams>,
) -> ApiResult<Json<CaseList>> {
    let statuses = parse_statuses(
        p.status.as_deref(),
        &["open", "in_review", "awaiting_response"],
    )?;
    let me = user(&headers).ok();
    let assignee = match p.assignee.as_deref().filter(|s| !s.is_empty()) {
        Some("me") => Some(
            me.clone()
                .ok_or_else(|| ApiError::BadRequest("X-User header required".into()))?,
        ),
        other => other.map(str::to_owned),
    };
    let items = case_db::list(
        &st.db,
        &case_db::ListFilter {
            statuses: &statuses,
            assignee: assignee.as_deref(),
            form_type: p.form_type.as_deref().filter(|s| !s.is_empty()),
            overdue_on: p.overdue.unwrap_or(false).then(today),
            limit: p.limit.unwrap_or(100).clamp(1, 500),
            offset: p.offset.unwrap_or(0).max(0),
        },
    )
    .await?;
    let mine = match &me {
        Some(u) => case_db::assigned_count(&st.db, u).await?,
        None => 0,
    };
    Ok(Json(CaseList {
        items,
        counts: case_db::status_counts(&st.db).await?,
        overdue: case_db::overdue_count(&st.db, today()).await?,
        mine,
    }))
}

#[derive(Deserialize, ToSchema)]
struct CreateCase {
    /// Primary document: the benefit form.
    document_id: Uuid,
    /// Benefit form type; defaults to the document's receipted or recognized form.
    form_type: Option<String>,
    /// Defaults to the document's `receipt_number` label, else generated.
    receipt_number: Option<String>,
    /// `YYYY-MM-DD`; defaults to the `received_date` label, else the upload date.
    received_date: Option<String>,
}

/// Open a case for a stored document.
#[utoipa::path(post, path = "/v1/cases", request_body = CreateCase,
    responses((status = 201, body = Case), (status = 400), (status = 404), (status = 409)))]
async fn create_case(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<CreateCase>,
) -> ApiResult<(StatusCode, Json<Case>)> {
    let actor = user(&headers)?;
    let doc = st.storage.document(b.document_id).await?;
    let values = case_ops::form_values(&st.storage, &doc).await?;
    let form_type = b
        .form_type
        .clone()
        .or_else(|| values.as_ref().and_then(|v| v.form_type.clone()))
        .ok_or_else(|| ApiError::BadRequest("form type unknown; pass form_type".into()))?;
    let benefit = st
        .benefits
        .get(&form_type)
        .ok_or_else(|| ApiError::BadRequest(format!("{form_type:?} is not a benefit form")))?;
    let receipt_number = match &b.receipt_number {
        Some(r) => Some(case::normalize_receipt_number(r).ok_or_else(|| {
            ApiError::BadRequest("receipt_number must be 3 letters and 10 digits".into())
        })?),
        None => case_ops::receipt_label(&doc),
    };
    let received = match &b.received_date {
        Some(d) => case::parse_date(d).map_err(|e| rejected("received_date", &e))?,
        None => case_ops::received_date(&doc),
    };
    let facts = values
        .as_ref()
        .filter(|v| v.form_type.as_deref() == Some(form_type.as_str()))
        .map(|v| benefit.derive_facts(v.template(&st.templates), &v.values))
        .unwrap_or_default();

    let mut tx = st.db.begin().await?;
    if let Some((c, _)) = case_db::for_document(&mut tx, b.document_id).await? {
        return Err(ApiError::Conflict(json!({
            "error": "conflict", "message": "document already belongs to a case", "case_id": c.id
        })));
    }
    if let Some(rn) = &receipt_number
        && let Some(c) = case_db::by_receipt(&mut tx, rn).await?
    {
        return Err(ApiError::Conflict(json!({
            "error": "conflict", "message": "receipt number in use", "case_id": c.id
        })));
    }
    let c = case_ops::open_case(
        &mut tx,
        &st.cfg.receipt_prefix,
        &case_ops::Opening {
            receipt_number: receipt_number.as_deref(),
            form_type: &form_type,
            document_id: b.document_id,
            filename: doc.source_filename.as_deref(),
            received_date: received,
            facts: &facts,
            actor: Some(&actor),
        },
    )
    .await?;
    tx.commit().await?;
    metrics::counter!("cases_opened_total").increment(1);
    st.projector.spawn_sync(c.id);
    st.assigner.spawn_dispatch(c.id);
    crate::publish(
        &st.js,
        CASE_OPENED,
        b.document_id,
        CaseOpened {
            case_id: c.id,
            receipt_number: c.receipt_number.clone(),
            form_type: c.form_type.clone(),
            document_id: b.document_id,
        },
    )
    .await;
    Ok((StatusCode::CREATED, Json(c)))
}

#[derive(Serialize, ToSchema)]
struct DocumentView {
    #[serde(flatten)]
    link: CaseDocument,
    /// Storage still has the document.
    available: bool,
    form_type: Option<String>,
    receipt_artifact_id: Option<Uuid>,
    recognition_artifact_id: Option<Uuid>,
    /// Flagged fields in the latest recognition, when not yet receipted.
    requires_receipting: usize,
    active_task: Option<db::Task>,
}

#[derive(Serialize, ToSchema)]
struct HighlightValue {
    label: String,
    value: String,
}

#[derive(Serialize, ToSchema)]
struct CaseDetail {
    case: Case,
    benefit: Option<Benefit>,
    /// Form number of the case's form type, e.g. `I-765`.
    form_number: Option<String>,
    grounds: Vec<Ground>,
    documents: Vec<DocumentView>,
    notices: Vec<Notice>,
    /// Current `workbench.decision.v1`, when decided.
    #[schema(value_type = Option<Object>)]
    decision: Option<Value>,
    highlights: Vec<HighlightValue>,
    /// The primary form has final values for the case's form type.
    form_final: bool,
    response_overdue: bool,
    /// Pending workload requests on the case.
    requests: Vec<workload_db::Request>,
    /// Contact from the most recently added e-notification request (G-1145) on the case.
    e_notification: Option<ENotificationContact>,
}

#[derive(Serialize, ToSchema)]
struct ENotificationContact {
    document_id: Uuid,
    email: Option<String>,
    mobile_phone: Option<String>,
}

async fn document_view(
    st: &AppState,
    link: CaseDocument,
) -> ApiResult<(DocumentView, Option<FormValues>)> {
    let active_task = db::active_for_document(&st.db, link.document_id).await?;
    let Some(doc) = case_ops::document(&st.storage, link.document_id).await? else {
        let view = DocumentView {
            link,
            available: false,
            form_type: None,
            receipt_artifact_id: None,
            recognition_artifact_id: None,
            requires_receipting: 0,
            active_task,
        };
        return Ok((view, None));
    };
    let values = case_ops::form_values(&st.storage, &doc).await?;
    let view = DocumentView {
        available: true,
        form_type: values.as_ref().and_then(|v| v.form_type.clone()),
        receipt_artifact_id: doc.latest(receipt::SCHEMA).map(|a| a.id),
        recognition_artifact_id: doc.latest(case_ops::RECOGNITION_SCHEMA).map(|a| a.id),
        requires_receipting: values.as_ref().map_or(0, |v| v.requires_receipting),
        active_task,
        link,
    };
    Ok((view, values))
}

/// E-notification contact from the latest-added case document of the configured form type.
fn e_notification(
    st: &AppState,
    docs: &[(DocumentView, Option<FormValues>)],
) -> Option<ENotificationContact> {
    let en = st.benefits.e_notification.as_ref()?;
    docs.iter()
        .filter(|(d, _)| d.form_type.as_deref() == Some(en.form_type.as_str()))
        .max_by_key(|(d, _)| d.link.added_at)
        .and_then(|(d, v)| {
            let v = v.as_ref()?;
            let template = v.template(&st.templates);
            let get = |f: &crate::benefits::FieldRef| {
                v.values
                    .get(f.resolve(template, &v.values)?)
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
            };
            Some(ENotificationContact {
                document_id: d.link.document_id,
                email: get(&en.email),
                mobile_phone: en.mobile_phone.as_ref().and_then(get),
            })
        })
}

#[utoipa::path(get, path = "/v1/cases/{id}", responses((status = 200, body = CaseDetail), (status = 404)))]
async fn get_case(State(st): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<CaseDetail>> {
    let c = load(&st, id).await?;
    let links = case_db::documents(&st.db, id).await?;
    let views =
        futures::future::try_join_all(links.into_iter().map(|l| document_view(&st, l))).await?;
    let e_notification = e_notification(&st, &views);
    let documents = views.into_iter().map(|(d, _)| d).collect();
    let primary = primary_values(&st, &c).await?;
    let benefit = st.benefits.get(&c.form_type).cloned();
    let highlights = match (&benefit, &primary) {
        (Some(b), Some(v)) if v.form_type.as_deref() == Some(c.form_type.as_str()) => b
            .highlight_values(&v.values)
            .into_iter()
            .map(|(label, value)| HighlightValue { label, value })
            .collect(),
        _ => vec![],
    };
    let decision = match c.decision_artifact_id {
        Some(a) => Some(
            serde_json::from_slice(&st.storage.content(a).await?).map_err(anyhow::Error::from)?,
        ),
        None => None,
    };
    Ok(Json(CaseDetail {
        form_final: primary
            .as_ref()
            .is_some_and(|v| v.is_final_for(&c.form_type)),
        response_overdue: overdue(&c),
        requests: workload_db::requests(
            &st.db,
            &workload_db::RequestFilter {
                statuses: &["pending".to_owned()],
                worker: None,
                case_id: Some(id),
                limit: 100,
            },
        )
        .await?,
        notices: case_db::notices(&st.db, id).await?,
        grounds: st.benefits.grounds.clone(),
        benefit,
        documents,
        decision,
        highlights,
        e_notification,
        form_number: st
            .templates
            .latest(&c.form_type)
            .map(|t| t.form_number.clone()),
        case: c,
    }))
}

#[derive(Deserialize, ToSchema)]
struct UpdateCase {
    /// Fact key → value; `""` records a blank. Edited keys are no longer derived from the form.
    #[serde(default)]
    facts: BTreeMap<String, String>,
    /// Fact keys to derive from the form again.
    #[serde(default)]
    reset: Vec<String>,
    /// Benefit form type.
    form_type: Option<String>,
    /// `YYYY-MM-DD`.
    received_date: Option<String>,
    reason: Option<String>,
}

/// Edit case facts, form type or received date.
#[utoipa::path(patch, path = "/v1/cases/{id}", request_body = UpdateCase,
    responses((status = 200, body = Case), (status = 409), (status = 422, body = Rejection)))]
async fn update_case(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(b): Json<UpdateCase>,
) -> ApiResult<Json<Case>> {
    let actor = user(&headers)?;
    let mut problems = BTreeMap::new();
    let mut edits = BTreeMap::new();
    for (k, v) in &b.facts {
        match normalize_fact(k, v) {
            Ok(n) => {
                edits.insert(k.clone(), n);
            }
            Err(e) => {
                problems.insert(format!("facts.{k}"), e);
            }
        }
    }
    for k in &b.reset {
        if crate::benefits::fact_type(k).is_none() {
            problems.insert(format!("reset.{k}"), "unknown fact".into());
        }
    }
    if let Some(ft) = &b.form_type
        && st.benefits.get(ft).is_none()
    {
        problems.insert("form_type".into(), "not a benefit form".into());
    }
    let received = match &b.received_date {
        Some(d) => match case::parse_date(d) {
            Ok(d) => Some(case::format_date(d)),
            Err(e) => {
                problems.insert("received_date".into(), e);
                None
            }
        },
        None => None,
    };
    if !problems.is_empty() {
        return Err(ApiError::Rejected(Rejection {
            message: format!("{} value(s) rejected", problems.len()),
            fields: problems,
        }));
    }
    let before = load(&st, id).await?;
    let derived = if b.reset.is_empty() {
        BTreeMap::new()
    } else {
        let ft = b.form_type.as_deref().unwrap_or(&before.form_type);
        match (st.benefits.get(ft), primary_values(&st, &before).await?) {
            (Some(bf), Some(v)) if v.form_type.as_deref() == Some(ft) => {
                bf.derive_facts(v.template(&st.templates), &v.values)
            }
            _ => BTreeMap::new(),
        }
    };

    let mut tx = st.db.begin().await?;
    let retyping = b
        .form_type
        .as_ref()
        .is_some_and(|ft| ft != &before.form_type);
    if retyping {
        workload_db::lock_assignments(&mut tx).await?;
    }
    let c = lock(&mut tx, id).await?;
    ensure_undecided(&c)?;
    ensure_can_edit(&c, &actor)?;
    let mut facts = c.facts.0.clone();
    let mut overridden = c.overridden.clone();
    for k in &b.reset {
        overridden.retain(|o| o != k);
        match derived.get(k) {
            Some(v) => facts.insert(k.clone(), v.clone()),
            None => facts.remove(k),
        };
    }
    for (k, v) in &edits {
        if !overridden.contains(k) {
            overridden.push(k.clone());
        }
        if v.is_empty() {
            facts.remove(k);
        } else {
            facts.insert(k.clone(), v.clone());
        }
    }
    overridden.sort();
    let mut changes = serde_json::Map::new();
    if facts != c.facts.0 || overridden != c.overridden {
        for k in c.facts.0.keys().chain(facts.keys()) {
            let (old, new) = (c.facts.0.get(k), facts.get(k));
            if old != new {
                changes.insert(k.clone(), json!({ "from": old, "to": new }));
            }
        }
        case_db::set_facts(&mut tx, id, &facts, &overridden).await?;
    }
    let mut unassigned = None;
    if let Some(ft) = &b.form_type
        && ft != &c.form_type
    {
        if !retyping {
            // Changed since `before`: the assignment lock must precede the case lock.
            return Err(conflict("case changed; retry"));
        }
        let rt = workload_ops::retype(&mut tx, &c, ft, Some(&actor)).await?;
        changes.insert("form_type".into(), json!({ "from": c.form_type, "to": ft }));
        if !rt.cleared.is_empty() {
            changes.insert("criteria_cleared".into(), json!(rt.cleared));
        }
        unassigned = rt.change;
    }
    if let Some(d) = &received
        && d != &c.received_date
    {
        case_db::set_received_date(&mut tx, id, d).await?;
        changes.insert(
            "received_date".into(),
            json!({ "from": c.received_date, "to": d }),
        );
    }
    if !changes.is_empty() {
        case_db::log(
            &mut tx,
            id,
            Some(&actor),
            "edited",
            json!({ "changes": changes, "reset": b.reset, "reason": b.reason }),
        )
        .await?;
    }
    tx.commit().await?;
    st.projector.spawn_sync(id);
    if retyping {
        after_assignment(&st, unassigned, Some(id)).await;
    }
    Ok(Json(load(&st, id).await?))
}

#[derive(Deserialize, ToSchema, Default)]
struct AssignBody {
    /// Defaults to the requesting user.
    assignee: Option<String>,
    /// Exceed the worker's `max_cases`.
    #[serde(default)]
    force: bool,
}

/// Assign or reassign a case (manager). The assignee must be a worker skilled for the form.
#[utoipa::path(post, path = "/v1/cases/{id}/assign", request_body = AssignBody,
    responses((status = 200, body = Case), (status = 403), (status = 409), (status = 422, body = Rejection)))]
async fn assign(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    body: Option<Json<AssignBody>>,
) -> ApiResult<Json<Case>> {
    let actor = manager(&headers)?;
    let b = body.map(|b| b.0).unwrap_or_default();
    let target = match b.assignee.as_deref() {
        Some(a) => workload::normalize_worker(a)
            .ok_or_else(|| rejected("assignee", "names are 1 to 64 characters"))?,
        None => actor.clone(),
    };
    let mut tx = st.db.begin().await?;
    workload_db::lock_assignments(&mut tx).await?;
    let c = lock(&mut tx, id).await?;
    ensure_undecided(&c)?;
    let change = workload_ops::assign_checked(&mut tx, &c, &target, b.force, &actor, Via::Manager)
        .await?
        .map_err(|r| refused(&r, &target, &c.form_type))?;
    tx.commit().await?;
    after_assignment(&st, change, None).await;
    Ok(Json(load(&st, id).await?))
}

/// Publishes an assignee change, dispatches `redispatch` and refills the worker the case was taken
/// from.
async fn after_assignment(st: &AppState, change: Option<Change>, redispatch: Option<Uuid>) {
    let previous = match change {
        Some(ch) => {
            st.assigner.published(std::slice::from_ref(&ch)).await;
            ch.previous
        }
        None => None,
    };
    if redispatch.is_some() || previous.is_some() {
        st.assigner
            .spawn_rebalance(redispatch, previous.into_iter().collect());
    }
}

/// Return a case to the unassigned pool (manager). Assignment rules may give it to another worker.
#[utoipa::path(post, path = "/v1/cases/{id}/unassign",
    responses((status = 200, body = Case), (status = 403), (status = 409)))]
async fn unassign(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> ApiResult<Json<Case>> {
    let actor = manager(&headers)?;
    let mut tx = st.db.begin().await?;
    workload_db::lock_assignments(&mut tx).await?;
    let c = lock(&mut tx, id).await?;
    ensure_undecided(&c)?;
    let change = workload_ops::set(&mut tx, &c, None, Some(&actor), Via::Manager).await?;
    tx.commit().await?;
    after_assignment(&st, change, Some(id)).await;
    Ok(Json(load(&st, id).await?))
}

#[derive(Deserialize, ToSchema)]
struct CriteriaBody {
    /// Criterion id → assessment, or `null` to clear.
    #[schema(value_type = Object)]
    criteria: BTreeMap<String, Option<AssessmentInput>>,
}

/// Record criterion assessments (partial update).
#[utoipa::path(put, path = "/v1/cases/{id}/criteria", request_body = CriteriaBody,
    responses((status = 200, body = Case), (status = 409), (status = 422, body = Rejection)))]
async fn save_criteria(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(b): Json<CriteriaBody>,
) -> ApiResult<Json<Case>> {
    let actor = user(&headers)?;
    let mut tx = st.db.begin().await?;
    let c = lock(&mut tx, id).await?;
    ensure_undecided(&c)?;
    ensure_assignee(&c, &actor)?;
    let bf = benefit(&st, &c)?;
    let next = case::apply_assessments(
        bf,
        &c.criteria.0,
        &b.criteria,
        &actor,
        OffsetDateTime::now_utc(),
    )
    .map_err(ApiError::Rejected)?;
    if next != c.criteria.0 {
        case_db::set_criteria(&mut tx, id, &next).await?;
        let changed: BTreeMap<&String, Option<CriterionResult>> = b
            .criteria
            .keys()
            .filter(|k| next.get(*k) != c.criteria.0.get(*k))
            .map(|k| (k, next.get(k).map(|a| a.result)))
            .collect();
        case_db::log(
            &mut tx,
            id,
            Some(&actor),
            "assessed",
            json!({ "criteria": changed }),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Json(load(&st, id).await?))
}

#[derive(Serialize)]
struct NoticeArtifact<'a> {
    schema: &'static str,
    notice_id: Uuid,
    case_id: Uuid,
    receipt_number: &'a str,
    form_type: &'a str,
    benefit: &'a str,
    kind: NoticeKind,
    items: &'a [String],
    message: Option<&'a str>,
    issued_by: &'a str,
    #[serde(with = "time::serde::rfc3339")]
    issued_at: OffsetDateTime,
    due_date: String,
    /// Addressee facts at issue time.
    facts: &'a BTreeMap<String, String>,
}

/// Issue a request for evidence or notice of intent to deny.
#[utoipa::path(post, path = "/v1/cases/{id}/notices", request_body = NoticeInput,
    responses((status = 201, body = Notice), (status = 409), (status = 422, body = Rejection)))]
async fn issue_notice(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(n): Json<NoticeInput>,
) -> ApiResult<(StatusCode, Json<Notice>)> {
    let actor = user(&headers)?;
    let c = load(&st, id).await?;
    ensure_undecided(&c)?;
    ensure_assignee(&c, &actor)?;
    let days = match n.kind {
        NoticeKind::Rfe => st.cfg.rfe_response_days,
        NoticeKind::Noid => st.cfg.noid_response_days,
    };
    let (items, due) =
        case::check_notice(&n, status(&c), today(), days).map_err(ApiError::Rejected)?;
    let message = n
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    let bf = benefit(&st, &c)?;
    let notice_id = Uuid::now_v7();
    let due_s = case::format_date(due);
    let body = serde_json::to_vec(&NoticeArtifact {
        schema: NOTICE_SCHEMA,
        notice_id,
        case_id: c.id,
        receipt_number: &c.receipt_number,
        form_type: &c.form_type,
        benefit: &bf.name,
        kind: n.kind,
        items: &items,
        message,
        issued_by: &actor,
        issued_at: OffsetDateTime::now_utc(),
        due_date: due_s.clone(),
        facts: &c.facts.0,
    })
    .map_err(anyhow::Error::from)?;
    let parent = st
        .storage
        .document(c.primary_document_id)
        .await?
        .source()
        .map(|a| a.id)
        .ok_or(ApiError::NotFound)?;
    let art = st
        .storage
        .create_artifact_as(
            c.primary_document_id,
            NOTICE_SCHEMA,
            PRODUCER,
            parent,
            &actor,
            body,
        )
        .await?;

    let mut tx = st.db.begin().await?;
    let locked = lock(&mut tx, id).await?;
    ensure_assignee(&locked, &actor)?;
    if status(&locked) != Status::InReview {
        return Err(conflict(format!("case is {}", locked.status)));
    }
    let notice = case_db::insert_notice(
        &mut tx,
        &case_db::NewNotice {
            id: notice_id,
            case_id: id,
            kind: n.kind.as_str(),
            items: &items,
            message,
            issued_by: &actor,
            due_date: &due_s,
            artifact_id: art.id,
        },
    )
    .await?;
    case_db::set_status(&mut tx, id, Status::AwaitingResponse.as_str(), Some(&due_s)).await?;
    case_db::log(
        &mut tx,
        id,
        Some(&actor),
        "notice_issued",
        json!({ "notice_id": notice_id, "kind": n.kind, "items": items.len(), "due_date": due_s, "artifact_id": art.id }),
    )
    .await?;
    tx.commit().await?;
    metrics::counter!("case_notices_total", "kind" => n.kind.as_str()).increment(1);
    st.projector.spawn_sync(id);
    crate::publish(
        &st.js,
        NOTICE_ISSUED,
        c.primary_document_id,
        NoticeIssued {
            case_id: id,
            receipt_number: c.receipt_number.clone(),
            notice_id,
            kind: n.kind.as_str().into(),
            artifact_id: art.id,
            due_date: due_s,
        },
    )
    .await;
    Ok((StatusCode::CREATED, Json(notice)))
}

#[derive(Deserialize, ToSchema)]
struct ResponseBody {
    note: Option<String>,
    /// Stored documents received in response; attached with role `response`.
    #[serde(default)]
    document_ids: Vec<Uuid>,
}

/// Record the response to the outstanding notice; the case returns to review. Unassigned, or
/// assigned to the requesting user.
#[utoipa::path(post, path = "/v1/cases/{id}/notices/{nid}/response", request_body = ResponseBody,
    responses((status = 200, body = Case), (status = 404), (status = 409)))]
async fn record_response(
    State(st): State<AppState>,
    Path((id, nid)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    Json(b): Json<ResponseBody>,
) -> ApiResult<Json<Case>> {
    let actor = user(&headers)?;
    let mut docs = Vec::new();
    for d in &b.document_ids {
        docs.push(st.storage.document(*d).await?);
    }
    let note = b.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let mut tx = st.db.begin().await?;
    let c = lock(&mut tx, id).await?;
    ensure_can_edit(&c, &actor)?;
    outstanding(&mut tx, &c, nid).await?;
    for d in &docs {
        if let Some((other, _)) = case_db::for_document(&mut tx, d.id).await? {
            if other.id != id {
                return Err(ApiError::Conflict(json!({
                    "error": "conflict", "message": format!("document {} belongs to another case", d.id), "case_id": other.id
                })));
            }
            continue;
        }
        case_db::add_document(
            &mut tx,
            id,
            d.id,
            "response",
            d.source_filename.as_deref(),
            Some(&actor),
        )
        .await?;
        case_db::log(
            &mut tx,
            id,
            Some(&actor),
            "document_added",
            json!({ "document_id": d.id, "role": "response", "filename": d.source_filename }),
        )
        .await?;
    }
    case_ops::record_response(&mut tx, &c, Some(&actor), note).await?;
    tx.commit().await?;
    st.projector.spawn_sync(id);
    Ok(Json(load(&st, id).await?))
}

/// The outstanding notice `nid` of a locked case.
async fn outstanding(conn: &mut PgConnection, c: &Case, nid: Uuid) -> ApiResult<Notice> {
    match case_db::open_notice(conn, c.id).await? {
        Some(n) if status(c) == Status::AwaitingResponse && n.id == nid => Ok(n),
        _ => Err(conflict("no outstanding notice with this id")),
    }
}

/// Extend the response window of the outstanding notice, also after it has passed. Assignee only.
#[utoipa::path(post, path = "/v1/cases/{id}/notices/{nid}/extension", request_body = ExtensionInput,
    responses((status = 200, body = Case), (status = 409), (status = 422, body = Rejection)))]
async fn extend_notice(
    State(st): State<AppState>,
    Path((id, nid)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    Json(e): Json<ExtensionInput>,
) -> ApiResult<Json<Case>> {
    let actor = user(&headers)?;
    let mut tx = st.db.begin().await?;
    let c = lock(&mut tx, id).await?;
    ensure_assignee(&c, &actor)?;
    let n = outstanding(&mut tx, &c, nid).await?;
    let current = case::parse_date(&n.due_date).map_err(anyhow::Error::msg)?;
    let (due, reason) = case::check_extension(&e, current, today()).map_err(ApiError::Rejected)?;
    let due_s = case::format_date(due);
    case_db::extend_notice(&mut tx, nid, &due_s).await?;
    case_db::set_status(&mut tx, id, Status::AwaitingResponse.as_str(), Some(&due_s)).await?;
    case_db::log(
        &mut tx,
        id,
        Some(&actor),
        "notice_extended",
        json!({ "notice_id": nid, "kind": n.kind, "from": n.due_date, "to": due_s, "reason": reason }),
    )
    .await?;
    tx.commit().await?;
    metrics::counter!("case_notice_extensions_total", "kind" => n.kind.clone()).increment(1);
    st.projector.spawn_sync(id);
    crate::publish(
        &st.js,
        NOTICE_EXTENDED,
        c.primary_document_id,
        NoticeExtended {
            case_id: id,
            receipt_number: c.receipt_number.clone(),
            notice_id: nid,
            kind: n.kind,
            previous_due_date: n.due_date,
            due_date: due_s,
            extended_by: actor,
            reason,
        },
    )
    .await;
    Ok(Json(load(&st, id).await?))
}

#[derive(Deserialize, ToSchema)]
struct RescindBody {
    /// E.g. issued in error or to the wrong party.
    reason: String,
}

/// Withdraw the outstanding notice; the case returns to review. Assignee only.
#[utoipa::path(post, path = "/v1/cases/{id}/notices/{nid}/rescind", request_body = RescindBody,
    responses((status = 200, body = Case), (status = 409), (status = 422, body = Rejection)))]
async fn rescind_notice(
    State(st): State<AppState>,
    Path((id, nid)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    Json(b): Json<RescindBody>,
) -> ApiResult<Json<Case>> {
    let actor = user(&headers)?;
    let reason = case::check_rescission(&b.reason).map_err(ApiError::Rejected)?;
    let mut tx = st.db.begin().await?;
    let c = lock(&mut tx, id).await?;
    ensure_assignee(&c, &actor)?;
    let n = outstanding(&mut tx, &c, nid).await?;
    case_db::rescind_notice(&mut tx, nid, &reason, &actor).await?;
    case_db::set_status(
        &mut tx,
        id,
        Status::working(c.assignee.as_deref()).as_str(),
        None,
    )
    .await?;
    case_db::log(
        &mut tx,
        id,
        Some(&actor),
        "notice_rescinded",
        json!({ "notice_id": nid, "kind": n.kind, "reason": reason }),
    )
    .await?;
    tx.commit().await?;
    metrics::counter!("case_notice_rescissions_total", "kind" => n.kind.clone()).increment(1);
    st.projector.spawn_sync(id);
    crate::publish(
        &st.js,
        NOTICE_RESCINDED,
        c.primary_document_id,
        NoticeRescinded {
            case_id: id,
            receipt_number: c.receipt_number.clone(),
            notice_id: nid,
            kind: n.kind,
            rescinded_by: actor,
            reason,
        },
    )
    .await;
    Ok(Json(load(&st, id).await?))
}

#[derive(Serialize)]
struct CriterionRecord<'a> {
    id: &'a str,
    label: &'a str,
    required: bool,
    result: Option<CriterionResult>,
    note: Option<&'a str>,
    assessed_by: Option<&'a str>,
    #[serde(with = "time::serde::rfc3339::option")]
    assessed_at: Option<OffsetDateTime>,
}

#[derive(Serialize)]
struct NoticeRecord<'a> {
    notice_id: Uuid,
    kind: &'a str,
    #[serde(with = "time::serde::rfc3339")]
    issued_at: OffsetDateTime,
    due_date: &'a str,
    original_due_date: Option<&'a str>,
    #[serde(with = "time::serde::rfc3339::option")]
    responded_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    rescinded_at: Option<OffsetDateTime>,
    artifact_id: Uuid,
}

#[derive(Serialize)]
struct DecisionDocument<'a> {
    document_id: Uuid,
    role: &'a str,
    filename: Option<&'a str>,
}

#[derive(Serialize)]
struct DecisionArtifact<'a> {
    schema: &'static str,
    case_id: Uuid,
    receipt_number: &'a str,
    form_type: &'a str,
    form_number: Option<&'a str>,
    benefit: &'a str,
    outcome: Outcome,
    grounds: Vec<&'a Ground>,
    closure_reason: Option<ClosureReason>,
    rationale: &'a str,
    criteria: Vec<CriterionRecord<'a>>,
    decided_by: &'a str,
    #[serde(with = "time::serde::rfc3339")]
    decided_at: OffsetDateTime,
    received_date: &'a str,
    facts: &'a BTreeMap<String, String>,
    /// Primary form values the decision rests on (receipt or recognition artifact).
    form_artifact_id: Option<Uuid>,
    documents: Vec<DecisionDocument<'a>>,
    notices: Vec<NoticeRecord<'a>>,
    /// Open inference findings at decision time; absent when inference is not configured, or was
    /// unavailable for a denial or withdrawal.
    findings: Option<&'a [findings::Finding]>,
    /// Decision artifact this one replaces after a reopening.
    supersedes: Option<Uuid>,
}

#[derive(Serialize, ToSchema)]
struct Decided {
    case: Case,
    decision_artifact_id: Uuid,
    version: i32,
}

/// Render the final decision: approve, deny, record a withdrawal, or close administratively.
/// Decisions require the assignee, skilled for the form; a manager may also close any undecided
/// case, assigned or not.
#[utoipa::path(post, path = "/v1/cases/{id}/decision", request_body = DecisionInput,
    responses((status = 200, body = Decided), (status = 409), (status = 422, body = Rejection)))]
async fn decide(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(d): Json<DecisionInput>,
) -> ApiResult<Json<Decided>> {
    let actor = user(&headers)?;
    let c = load(&st, id).await?;
    ensure_undecided(&c)?;
    let as_manager = d.outcome == Outcome::Closed
        && manager(&headers).is_ok()
        && c.assignee.as_deref() != Some(actor.as_str());
    if !as_manager {
        ensure_assignee(&c, &actor)?;
        // The form type can change after assignment (receipting); adjudication needs the skill.
        let e = workload_ops::eligibility(&mut *st.db.acquire().await?, &c, &actor).await?;
        if !e.skilled {
            return Err(conflict(format!(
                "you are not skilled for {}; ask a manager to reassign the case",
                c.form_type
            )));
        }
    }
    let bf = benefit(&st, &c)?;
    let primary = primary_values(&st, &c).await?;
    let has_form_values = primary
        .as_ref()
        .is_some_and(|v| v.is_final_for(&c.form_type));
    let open_findings = findings::at_decision(&st, id, d.outcome).await?;
    let check = |c: &Case| {
        case::check_decision(
            &DecisionContext {
                benefits: &st.benefits,
                benefit: bf,
                status: status(c),
                criteria: &c.criteria.0,
                open_tasks: c.open_tasks,
                has_form_values,
                response_overdue: overdue(c),
                open_findings: open_findings.as_deref().map_or(0, findings::blocking),
            },
            &d,
        )
        .map_err(ApiError::Rejected)
    };
    check(&c)?;

    let docs = case_db::documents(&st.db, id).await?;
    let notices = case_db::notices(&st.db, id).await?;
    let supersedes = case_db::history(&st.db, id)
        .await?
        .into_iter()
        .rev()
        .find(|e| e.action == "reopened")
        .and_then(|e| {
            e.details["supersedes"]
                .as_str()
                .and_then(|s| s.parse().ok())
        });
    let now = OffsetDateTime::now_utc();
    let rationale = d.rationale.trim();
    let artifact = DecisionArtifact {
        schema: DECISION_SCHEMA,
        case_id: c.id,
        receipt_number: &c.receipt_number,
        form_type: &c.form_type,
        form_number: st
            .templates
            .get(&c.form_type)
            .map(|t| t.form_number.as_str()),
        benefit: &bf.name,
        outcome: d.outcome,
        closure_reason: d.closure_reason,
        grounds: d
            .grounds
            .iter()
            .filter_map(|g| st.benefits.ground(g))
            .collect(),
        rationale,
        criteria: bf
            .criteria
            .iter()
            .map(|cr| {
                let a = c.criteria.0.get(&cr.id);
                CriterionRecord {
                    id: &cr.id,
                    label: &cr.label,
                    required: cr.required,
                    result: a.map(|a| a.result),
                    note: a.and_then(|a| a.note.as_deref()),
                    assessed_by: a.map(|a| a.by.as_str()),
                    assessed_at: a.map(|a| a.at),
                }
            })
            .collect(),
        decided_by: &actor,
        decided_at: now,
        received_date: &c.received_date,
        facts: &c.facts.0,
        form_artifact_id: primary.as_ref().map(|v| v.artifact_id),
        documents: docs
            .iter()
            .map(|x| DecisionDocument {
                document_id: x.document_id,
                role: &x.role,
                filename: x.filename.as_deref(),
            })
            .collect(),
        notices: notices
            .iter()
            .map(|n| NoticeRecord {
                notice_id: n.id,
                kind: &n.kind,
                issued_at: n.issued_at,
                due_date: &n.due_date,
                original_due_date: n.original_due_date.as_deref(),
                responded_at: n.responded_at,
                rescinded_at: n.rescinded_at,
                artifact_id: n.artifact_id,
            })
            .collect(),
        findings: open_findings.as_deref(),
        supersedes,
    };
    let body = serde_json::to_vec(&artifact).map_err(anyhow::Error::from)?;
    let parent = primary
        .as_ref()
        .map(|v| v.artifact_id)
        .or(st
            .storage
            .document(c.primary_document_id)
            .await?
            .source()
            .map(|a| a.id))
        .ok_or(ApiError::NotFound)?;
    let art = st
        .storage
        .create_artifact_as(
            c.primary_document_id,
            DECISION_SCHEMA,
            PRODUCER,
            parent,
            &actor,
            body,
        )
        .await?;

    let mut tx = st.db.begin().await?;
    let locked = lock(&mut tx, id).await?;
    ensure_undecided(&locked)?;
    if !as_manager {
        ensure_assignee(&locked, &actor)?;
    }
    if locked.criteria.0 != c.criteria.0 {
        return Err(conflict(
            "assessment changed while deciding; review and resubmit",
        ));
    }
    check(&locked)?;
    case_db::decide(&mut tx, id, d.outcome.as_str(), &actor, art.id).await?;
    workload_db::cancel_requests(&mut tx, id, "case decided").await?;
    case_db::log(
        &mut tx,
        id,
        Some(&actor),
        "decided",
        json!({ "outcome": d.outcome, "grounds": d.grounds, "closure_reason": d.closure_reason, "artifact_id": art.id, "supersedes": supersedes }),
    )
    .await?;
    tx.commit().await?;
    metrics::counter!("case_decisions_total", "outcome" => d.outcome.as_str()).increment(1);
    st.projector.spawn_sync(id);
    // The completed assignment frees capacity: give the officer their next case.
    st.assigner
        .spawn_rebalance(None, locked.assignee.into_iter().collect());
    crate::publish(
        &st.js,
        CASE_DECIDED,
        c.primary_document_id,
        CaseDecided {
            case_id: id,
            receipt_number: c.receipt_number.clone(),
            form_type: c.form_type.clone(),
            outcome: d.outcome.as_str().into(),
            decided_by: actor,
            artifact_id: art.id,
            version: art.version,
        },
    )
    .await;
    Ok(Json(Decided {
        case: load(&st, id).await?,
        decision_artifact_id: art.id,
        version: art.version,
    }))
}

#[derive(Deserialize, ToSchema)]
struct ReopenBody {
    /// Motion, service error or supervisory review.
    reason: String,
}

/// Reopen a decided case for reconsideration; the requesting user becomes the assignee and must
/// be skilled for the form. Capacity is not checked.
#[utoipa::path(post, path = "/v1/cases/{id}/reopen", request_body = ReopenBody,
    responses((status = 200, body = Case), (status = 409), (status = 422, body = Rejection)))]
async fn reopen(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(b): Json<ReopenBody>,
) -> ApiResult<Json<Case>> {
    let actor = user(&headers)?;
    let reason = b.reason.trim();
    if reason.chars().count() < case::MIN_RATIONALE {
        return Err(rejected(
            "reason",
            &format!("at least {} characters", case::MIN_RATIONALE),
        ));
    }
    let mut tx = st.db.begin().await?;
    workload_db::lock_assignments(&mut tx).await?;
    let c = lock(&mut tx, id).await?;
    if status(&c) != Status::Decided {
        return Err(conflict(format!("case is {}", c.status)));
    }
    let e = workload_ops::eligibility(&mut tx, &c, &actor).await?;
    if let Err(r) = workload::check_assignment(&e, true) {
        return Err(refused(&r, &actor, &c.form_type));
    }
    case_db::reopen(&mut tx, id, &actor).await?;
    case_db::log(
        &mut tx,
        id,
        Some(&actor),
        "reopened",
        json!({ "reason": reason, "supersedes": c.decision_artifact_id, "previous_outcome": c.decision }),
    )
    .await?;
    tx.commit().await?;
    st.assigner.published(&[Change::reopened(&c, &actor)]).await;
    crate::publish(
        &st.js,
        CASE_REOPENED,
        c.primary_document_id,
        CaseReopened {
            case_id: id,
            receipt_number: c.receipt_number.clone(),
            reopened_by: actor,
            reason: reason.to_owned(),
            supersedes: c.decision_artifact_id,
        },
    )
    .await;
    Ok(Json(load(&st, id).await?))
}

#[derive(Deserialize, ToSchema)]
struct AddDocument {
    document_id: Uuid,
    /// `supporting` or `response`; defaults to `response` while a notice is outstanding.
    role: Option<String>,
}

/// Attach a stored document to an undecided case: unassigned, or assigned to the requesting user.
/// Uploads labeled with the receipt number are attached by the consumer regardless.
#[utoipa::path(post, path = "/v1/cases/{id}/documents", request_body = AddDocument,
    responses((status = 200, body = Case), (status = 404), (status = 409)))]
async fn add_document(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(b): Json<AddDocument>,
) -> ApiResult<Json<Case>> {
    let actor = user(&headers)?;
    if let Some(r) = &b.role
        && !matches!(r.as_str(), "supporting" | "response")
    {
        return Err(ApiError::BadRequest(
            "role must be supporting or response".into(),
        ));
    }
    let doc = st.storage.document(b.document_id).await?;
    let mut tx = st.db.begin().await?;
    let c = lock(&mut tx, id).await?;
    ensure_undecided(&c)?;
    ensure_can_edit(&c, &actor)?;
    if let Some((other, _)) = case_db::for_document(&mut tx, b.document_id).await? {
        return Err(ApiError::Conflict(json!({
            "error": "conflict", "message": "document already belongs to a case", "case_id": other.id
        })));
    }
    case_ops::attach(
        &mut tx,
        &c,
        b.document_id,
        doc.source_filename.as_deref(),
        b.role.as_deref(),
        Some(&actor),
    )
    .await?;
    tx.commit().await?;
    st.projector.spawn_sync(id);
    Ok(Json(load(&st, id).await?))
}

/// Detach a supporting or response document.
#[utoipa::path(delete, path = "/v1/cases/{id}/documents/{doc}",
    responses((status = 200, body = Case), (status = 404), (status = 409)))]
async fn remove_document(
    State(st): State<AppState>,
    Path((id, doc)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> ApiResult<Json<Case>> {
    let actor = user(&headers)?;
    let mut tx = st.db.begin().await?;
    let c = lock(&mut tx, id).await?;
    ensure_undecided(&c)?;
    ensure_can_edit(&c, &actor)?;
    if c.primary_document_id == doc {
        return Err(conflict("the primary document cannot be removed"));
    }
    if !case_db::remove_document(&mut tx, id, doc).await? {
        return Err(ApiError::NotFound);
    }
    case_db::log(
        &mut tx,
        id,
        Some(&actor),
        "document_removed",
        json!({ "document_id": doc }),
    )
    .await?;
    tx.commit().await?;
    st.projector.spawn_sync(id);
    Ok(Json(load(&st, id).await?))
}

#[derive(Deserialize, ToSchema)]
struct PrimaryBody {
    /// A supporting or response document of the case.
    document_id: Uuid,
    reason: String,
}

/// Replace the primary document with another case document; the former primary becomes
/// `supporting`. Facts are derived from the new primary's form values, keeping officer edits; a
/// document without form values keeps the current facts until it is receipted. Values for another
/// benefit form change the case's form type as a form type edit does. Unassigned, or assigned to
/// the requesting user.
#[utoipa::path(post, path = "/v1/cases/{id}/primary", request_body = PrimaryBody,
    responses((status = 200, body = Case), (status = 404), (status = 409), (status = 422, body = Rejection)))]
async fn change_primary(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(b): Json<PrimaryBody>,
) -> ApiResult<Json<Case>> {
    let actor = user(&headers)?;
    let reason = b.reason.trim();
    if reason.is_empty() || reason.chars().count() > 2000 {
        return Err(rejected(
            "reason",
            "give a reason of at most 2000 characters",
        ));
    }
    let before = load(&st, id).await?;
    ensure_undecided(&before)?;
    ensure_can_edit(&before, &actor)?;
    if before.primary_document_id == b.document_id {
        return Err(conflict("already the primary document"));
    }
    let doc = st.storage.document(b.document_id).await?;
    let values = case_ops::form_values(&st.storage, &doc).await?;
    let form_type = match values.as_ref().and_then(|v| v.form_type.as_deref()) {
        Some(ft) if st.benefits.get(ft).is_none() => {
            return Err(rejected(
                "document_id",
                &format!("{ft} is not a benefit form"),
            ));
        }
        Some(ft) => ft.to_owned(),
        None => before.form_type.clone(),
    };
    let retyping = form_type != before.form_type;

    let mut tx = st.db.begin().await?;
    if retyping {
        workload_db::lock_assignments(&mut tx).await?;
    }
    let c = lock(&mut tx, id).await?;
    ensure_undecided(&c)?;
    ensure_can_edit(&c, &actor)?;
    if c.primary_document_id != before.primary_document_id || c.form_type != before.form_type {
        return Err(conflict("case changed; retry"));
    }
    match case_db::for_document(&mut tx, b.document_id).await? {
        Some((dc, _)) if dc.id == id => {}
        _ => {
            return Err(rejected(
                "document_id",
                "attach the document to the case first",
            ));
        }
    }
    case_db::set_primary(&mut tx, id, b.document_id).await?;
    let (mut c, mut unassigned, mut changes) = (c, None, serde_json::Map::new());
    if retyping {
        let rt = workload_ops::retype(&mut tx, &c, &form_type, Some(&actor)).await?;
        changes.insert(
            "form_type".into(),
            json!({ "from": c.form_type, "to": form_type }),
        );
        if !rt.cleared.is_empty() {
            changes.insert("criteria_cleared".into(), json!(rt.cleared));
        }
        (c, unassigned) = (rt.case, rt.change);
    }
    if let Some(v) = values.as_ref().filter(|v| v.form_type.is_some())
        && case_ops::refresh_facts(
            &mut tx,
            &c,
            benefit(&st, &c)?,
            v.template(&st.templates),
            &v.values,
        )
        .await?
    {
        changes.insert("facts".into(), json!(true));
    }
    case_db::log(
        &mut tx,
        id,
        Some(&actor),
        "primary_changed",
        json!({ "from": before.primary_document_id, "to": b.document_id, "filename": doc.source_filename,
                "reason": reason, "changes": changes }),
    )
    .await?;
    // Before commit, holding the case lock: a failed write leaves the old primary in place.
    st.projector
        .retract(id, before.primary_document_id, b.document_id)
        .await?;
    tx.commit().await?;
    st.projector.spawn_sync(id);
    if retyping {
        after_assignment(&st, unassigned, Some(id)).await;
    }
    Ok(Json(load(&st, id).await?))
}

#[derive(Deserialize, ToSchema)]
struct AmendBody {
    /// Field id → corrected value; `null` = blank.
    #[schema(value_type = Object)]
    values: BTreeMap<String, Option<String>>,
    reason: String,
}

#[derive(Serialize, ToSchema)]
struct Amended {
    task: db::Task,
    receipt_artifact_id: Uuid,
    version: i32,
    stats: receipt::ReceiptStats,
    edited: usize,
}

/// Correct form values of a case document. Stores a new `receipting.form.v1` (mode `amend`)
/// recorded as a completed task.
#[utoipa::path(post, path = "/v1/cases/{id}/documents/{doc}/amend", request_body = AmendBody,
    responses((status = 200, body = Amended), (status = 400), (status = 409), (status = 422, body = Rejection)))]
async fn amend(
    State(st): State<AppState>,
    Path((id, doc_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    Json(b): Json<AmendBody>,
) -> ApiResult<Json<Amended>> {
    let actor = user(&headers)?;
    let reason = b.reason.trim();
    if reason.is_empty() || reason.chars().count() > 2000 {
        return Err(rejected(
            "reason",
            "give a reason of at most 2000 characters",
        ));
    }
    let c = load(&st, id).await?;
    ensure_undecided(&c)?;
    ensure_can_edit(&c, &actor)?;
    let mut conn = st.db.acquire().await?;
    match case_db::for_document(&mut conn, doc_id).await? {
        Some((dc, _)) if dc.id == id => {}
        _ => return Err(ApiError::NotFound),
    }
    drop(conn);
    if let Some(t) = db::active_for_document(&st.db, doc_id).await? {
        return Err(ApiError::Conflict(json!({
            "error": "conflict", "message": "complete the open receipting task first", "task_id": t.id
        })));
    }
    let doc = st.storage.document(doc_id).await?;
    let source = doc.source().ok_or(ApiError::NotFound)?.id;
    let values = case_ops::form_values(&st.storage, &doc)
        .await?
        .ok_or_else(|| {
            ApiError::BadRequest("document has no form values; receipt it first".into())
        })?;
    let template_id = values.template_id.clone().ok_or_else(|| {
        ApiError::BadRequest("form type unknown; receipt the document first".into())
    })?;
    let template = st
        .templates
        .get(&template_id)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown template {template_id:?}")))?;
    let (baseline, form_artifact_id) = match &values.receipt {
        Some(r) => (r.fields.clone(), r.form_artifact_id),
        None => {
            let rec: Recognition = load_recognition(&st, values.artifact_id).await?;
            let machine: HashMap<String, FieldResult> = rec
                .fields
                .iter()
                .map(|f| (f.id.clone(), f.clone()))
                .collect();
            let (fields, _) = receipt::resolve(&receipt::Submission {
                template,
                machine: &machine,
                mode: Mode::Review,
                values: &BTreeMap::new(),
            })
            .map_err(|_| ApiError::BadRequest("flagged fields must be receipted first".into()))?;
            (fields, Some(values.artifact_id))
        }
    };
    let (fields, stats, edited) =
        receipt::amend(template, &baseline, &b.values).map_err(ApiError::Rejected)?;
    let task_id = Uuid::now_v7();
    let r = Receipt {
        schema: receipt::SCHEMA.into(),
        document_id: doc_id,
        task_id,
        form_type: template.form_type().to_owned(),
        template_id: Some(template.id.clone()),
        form_number: template.form_number.clone(),
        edition: template.edition.clone(),
        mode: Mode::Amend,
        receipted_by: actor.clone(),
        receipted_at: OffsetDateTime::now_utc(),
        source_artifact_id: source,
        form_artifact_id,
        values: fields
            .iter()
            .filter_map(|f| f.value.clone().map(|v| (f.id.clone(), v)))
            .collect(),
        fields,
        stats: stats.clone(),
        amends: values.receipt.as_ref().map(|_| values.artifact_id),
        amendment_reason: Some(reason.to_owned()),
    };
    let body = serde_json::to_vec(&r).map_err(anyhow::Error::from)?;
    let art = st
        .storage
        .create_artifact_as(
            doc_id,
            receipt::SCHEMA,
            PRODUCER,
            values.artifact_id,
            &actor,
            body,
        )
        .await?;

    let mut tx = st.db.begin().await?;
    let locked = lock(&mut tx, id).await?;
    ensure_undecided(&locked)?;
    ensure_can_edit(&locked, &actor)?;
    let task = db::insert_amendment(
        &mut tx,
        task_id,
        &db::NewTask {
            document_id: doc_id,
            filename: doc.source_filename.as_deref(),
            form_type: Some(&template.id),
            kind: "amend",
            source_artifact_id: source,
            form_artifact_id,
            flagged_fields: 0,
            total_fields: template.fields.len() as i32,
        },
        &actor,
        art.id,
    )
    .await?;
    db::log(
        &mut tx,
        task_id,
        Some(&actor),
        "amended",
        json!({ "receipt_artifact_id": art.id, "amends": r.amends, "edited": edited, "reason": reason }),
    )
    .await?;
    case_db::log(
        &mut tx,
        id,
        Some(&actor),
        "amended",
        json!({ "document_id": doc_id, "receipt_artifact_id": art.id, "edited": edited, "reason": reason }),
    )
    .await?;
    let receipted = on_receipt(&st, &mut tx, &r, art.id, &actor).await?;
    tx.commit().await?;
    match receipted {
        Some(rc) => rc.after_commit(&st).await,
        None => st.projector.spawn_sync(id),
    }
    crate::publish(
        &st.js,
        RECEIPTING_COMPLETED,
        doc_id,
        ReceiptingCompleted {
            document_id: doc_id,
            task_id,
            artifact_id: art.id,
            version: art.version,
            form_type: r.form_type.clone(),
            mode: Mode::Amend.as_str().into(),
            receipted_by: actor,
            changed_fields: stats.changed,
        },
    )
    .await;
    Ok(Json(Amended {
        task,
        receipt_artifact_id: art.id,
        version: art.version,
        stats,
        edited,
    }))
}

#[derive(Deserialize, ToSchema)]
struct NoteBody {
    text: String,
}

/// Add an internal note to the case timeline.
#[utoipa::path(post, path = "/v1/cases/{id}/notes", request_body = NoteBody,
    responses((status = 201), (status = 404)))]
async fn add_note(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(b): Json<NoteBody>,
) -> ApiResult<StatusCode> {
    let actor = user(&headers)?;
    let text = b.text.trim();
    if text.is_empty() || text.chars().count() > 10_000 {
        return Err(rejected("text", "note must be 1 to 10000 characters"));
    }
    let mut tx = st.db.begin().await?;
    lock(&mut tx, id).await?;
    case_db::log(&mut tx, id, Some(&actor), "note", json!({ "text": text })).await?;
    tx.commit().await?;
    Ok(StatusCode::CREATED)
}

#[utoipa::path(get, path = "/v1/cases/{id}/history", responses((status = 200, body = Vec<CaseEvent>)))]
async fn case_history(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<CaseEvent>>> {
    load(&st, id).await?;
    Ok(Json(case_db::history(&st.db, id).await?))
}

#[derive(Serialize, ToSchema)]
struct CaseRef {
    id: Uuid,
    receipt_number: String,
    role: String,
}

#[derive(Serialize, ToSchema)]
struct DocumentForm {
    document_id: Uuid,
    filename: Option<String>,
    pages: Vec<PageInfo>,
    #[schema(value_type = Option<Object>)]
    receipt: Option<Receipt>,
    receipt_artifact_id: Option<Uuid>,
    recognition: Option<Recognition>,
    active_task: Option<db::Task>,
    case: Option<CaseRef>,
}

/// A document's pages, latest receipt and recognition, receipting task and case.
#[utoipa::path(get, path = "/v1/documents/{id}/form", responses((status = 200, body = DocumentForm), (status = 404)))]
async fn document_form(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<DocumentForm>> {
    let doc = st.storage.document(id).await?;
    let source = doc.source().ok_or(ApiError::NotFound)?.id;
    let pages = Pages::info(source_pdf(&st, source).await?).await?;
    let receipt_art = doc.latest(receipt::SCHEMA).map(|a| a.id);
    let receipt = match receipt_art {
        Some(a) => Some(
            serde_json::from_slice(&st.storage.content(a).await?).map_err(anyhow::Error::from)?,
        ),
        None => None,
    };
    let recognition = match doc.latest(case_ops::RECOGNITION_SCHEMA) {
        Some(a) => Some(load_recognition(&st, a.id).await?),
        None => None,
    };
    let mut conn = st.db.acquire().await?;
    let case = case_db::for_document(&mut conn, id)
        .await?
        .map(|(c, role)| CaseRef {
            id: c.id,
            receipt_number: c.receipt_number,
            role,
        });
    drop(conn);
    Ok(Json(DocumentForm {
        document_id: id,
        filename: doc.source_filename.clone(),
        pages,
        receipt,
        receipt_artifact_id: receipt_art,
        recognition,
        active_task: db::active_for_document(&st.db, id).await?,
        case,
    }))
}

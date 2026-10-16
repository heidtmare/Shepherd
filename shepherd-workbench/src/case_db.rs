//! Case tables in the `workbench` schema. Every change to projected state bumps `revision`;
//! `projection` writes `workbench.case.v1` until `indexed_revision` catches up.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;
use sqlx::{PgConnection, PgPool, types::Json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::case::Assessment;

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Case {
    pub id: Uuid,
    pub receipt_number: String,
    pub form_type: String,
    pub primary_document_id: Uuid,
    pub status: String,
    pub assignee: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub assigned_at: Option<OffsetDateTime>,
    #[schema(value_type = Object)]
    pub facts: Json<BTreeMap<String, String>>,
    pub overridden: Vec<String>,
    /// `YYYY-MM-DD`.
    pub received_date: String,
    /// `YYYY-MM-DD`; set while awaiting a response.
    pub response_due: Option<String>,
    #[schema(value_type = Object)]
    pub criteria: Json<BTreeMap<String, Assessment>>,
    pub decision: Option<String>,
    pub decided_by: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub decided_at: Option<OffsetDateTime>,
    pub decision_artifact_id: Option<Uuid>,
    pub case_artifact_id: Option<Uuid>,
    pub revision: i32,
    pub indexed_revision: i32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// Open or in-progress receipting tasks on the case's documents.
    pub open_tasks: i64,
}

const COLS: &str = "c.id, c.receipt_number, c.form_type, c.primary_document_id, c.status, c.assignee, c.assigned_at, \
     c.facts, c.overridden, c.received_date::text AS received_date, c.response_due::text AS response_due, c.criteria, \
     c.decision, c.decided_by, c.decided_at, c.decision_artifact_id, c.case_artifact_id, c.revision, c.indexed_revision, \
     c.created_at, c.updated_at, \
     (SELECT count(*) FROM workbench.tasks t JOIN workbench.case_documents d ON d.document_id = t.document_id \
      WHERE d.case_id = c.id AND t.status IN ('open', 'in_progress')) AS open_tasks";

pub struct NewCase<'a> {
    pub receipt_number: &'a str,
    pub form_type: &'a str,
    pub primary_document_id: Uuid,
    pub filename: Option<&'a str>,
    pub facts: &'a BTreeMap<String, String>,
    pub received_date: &'a str,
    pub actor: Option<&'a str>,
}

pub async fn next_receipt_seq(conn: &mut PgConnection) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT nextval('workbench.receipt_seq')")
        .fetch_one(conn)
        .await
}

/// Inserts the case and links its primary document.
pub async fn insert(conn: &mut PgConnection, n: &NewCase<'_>) -> sqlx::Result<Case> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO workbench.cases (id, receipt_number, form_type, primary_document_id, status, facts, received_date)
         VALUES ($1, $2, $3, $4, 'open', $5, $6::date)",
    )
    .bind(id)
    .bind(n.receipt_number)
    .bind(n.form_type)
    .bind(n.primary_document_id)
    .bind(Json(n.facts))
    .bind(n.received_date)
    .execute(&mut *conn)
    .await?;
    add_document(
        conn,
        id,
        n.primary_document_id,
        "primary",
        n.filename,
        n.actor,
    )
    .await?;
    Ok(fetch(conn, id).await?.expect("inserted case"))
}

async fn fetch(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Option<Case>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM workbench.cases c WHERE c.id = $1"
    )))
    .bind(id)
    .fetch_optional(conn)
    .await
}

pub async fn get(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<Case>> {
    fetch(&mut *pool.acquire().await?, id).await
}

pub async fn lock(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Option<Case>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM workbench.cases c WHERE c.id = $1 FOR UPDATE OF c"
    )))
    .bind(id)
    .fetch_optional(conn)
    .await
}

pub async fn by_receipt(conn: &mut PgConnection, receipt: &str) -> sqlx::Result<Option<Case>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM workbench.cases c WHERE c.receipt_number = $1"
    )))
    .bind(receipt)
    .fetch_optional(conn)
    .await
}

pub async fn by_a_number(pool: &PgPool, a_number: &str) -> sqlx::Result<Vec<Case>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM workbench.cases c
         WHERE c.facts ->> 'a_number' = $1 OR c.facts ->> 'beneficiary_a_number' = $1
         ORDER BY c.received_date DESC LIMIT 50"
    )))
    .bind(a_number)
    .fetch_all(pool)
    .await
}

/// The case a document belongs to, with its role.
pub async fn for_document(
    conn: &mut PgConnection,
    document_id: Uuid,
) -> sqlx::Result<Option<(Case, String)>> {
    let row: Option<CaseMatch> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS}, d.document_id AS matched_document_id, d.role AS matched_role
         FROM workbench.case_documents d JOIN workbench.cases c ON c.id = d.case_id
         WHERE d.document_id = $1"
    )))
    .bind(document_id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|m| (m.case, m.matched_role)))
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CaseMatch {
    #[sqlx(flatten)]
    pub case: Case,
    pub matched_document_id: Uuid,
    pub matched_role: String,
}

pub async fn for_documents(pool: &PgPool, ids: &[Uuid]) -> sqlx::Result<Vec<CaseMatch>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS}, d.document_id AS matched_document_id, d.role AS matched_role
         FROM workbench.case_documents d JOIN workbench.cases c ON c.id = d.case_id
         WHERE d.document_id = ANY($1)"
    )))
    .bind(ids)
    .fetch_all(pool)
    .await
}

pub struct ListFilter<'a> {
    /// Empty = all.
    pub statuses: &'a [String],
    pub assignee: Option<&'a str>,
    pub form_type: Option<&'a str>,
    /// Only cases awaiting a response due before this date, earliest due first.
    pub overdue_on: Option<time::Date>,
    pub limit: i64,
    pub offset: i64,
}

/// Worklist: oldest received first; decided cases newest decision first.
pub async fn list(pool: &PgPool, f: &ListFilter<'_>) -> sqlx::Result<Vec<Case>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM workbench.cases c
         WHERE (cardinality($1::text[]) = 0 OR c.status = ANY($1))
           AND ($2::text IS NULL OR c.assignee = $2)
           AND ($3::text IS NULL OR c.form_type = $3)
           AND ($6::date IS NULL OR (c.status = 'awaiting_response' AND c.response_due < $6))
         ORDER BY CASE WHEN c.status = 'decided' THEN 1 ELSE 0 END,
                  CASE WHEN $6::date IS NOT NULL THEN c.response_due END,
                  c.decided_at DESC NULLS LAST, c.received_date, c.created_at
         LIMIT $4 OFFSET $5"
    )))
    .bind(f.statuses)
    .bind(f.assignee)
    .bind(f.form_type)
    .bind(f.limit)
    .bind(f.offset)
    .bind(f.overdue_on)
    .fetch_all(pool)
    .await
}

/// Cases awaiting a response due before `today`.
pub async fn overdue_count(pool: &PgPool, today: time::Date) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) FROM workbench.cases WHERE status = 'awaiting_response' AND response_due < $1",
    )
    .bind(today)
    .fetch_one(pool)
    .await
}

pub struct SearchFilter<'a> {
    pub q: Option<&'a str>,
    pub statuses: &'a [String],
    pub form_type: Option<&'a str>,
    pub decision: Option<&'a str>,
    pub received_from: Option<&'a str>,
    pub received_to: Option<&'a str>,
    pub limit: i64,
    pub offset: i64,
}

/// Database fallback for discovery when shepherd-search is not configured or unavailable:
/// substring match on receipt number and names.
pub async fn search(pool: &PgPool, f: &SearchFilter<'_>) -> sqlx::Result<Vec<Case>> {
    let pattern = f.q.map(|q| {
        format!(
            "%{}%",
            q.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        )
    });
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM workbench.cases c
         WHERE ($1::text IS NULL OR c.receipt_number ILIKE $1
                OR concat_ws(' ', c.facts ->> 'given_name', c.facts ->> 'family_name',
                             c.facts ->> 'beneficiary_given_name', c.facts ->> 'beneficiary_family_name') ILIKE $1)
           AND (cardinality($2::text[]) = 0 OR c.status = ANY($2))
           AND ($3::text IS NULL OR c.form_type = $3)
           AND ($4::text IS NULL OR c.decision = $4)
           AND ($5::date IS NULL OR c.received_date >= $5::date)
           AND ($6::date IS NULL OR c.received_date <= $6::date)
         ORDER BY c.received_date DESC, c.id
         LIMIT $7 OFFSET $8"
    )))
    .bind(pattern)
    .bind(f.statuses)
    .bind(f.form_type)
    .bind(f.decision)
    .bind(f.received_from)
    .bind(f.received_to)
    .bind(f.limit)
    .bind(f.offset)
    .fetch_all(pool)
    .await
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Count {
    pub key: String,
    pub count: i64,
}

pub async fn status_counts(pool: &PgPool) -> sqlx::Result<Vec<Count>> {
    sqlx::query_as(
        "SELECT status AS key, count(*) AS count FROM workbench.cases GROUP BY status ORDER BY status",
    )
    .fetch_all(pool)
    .await
}

pub async fn assigned_count(pool: &PgPool, user: &str) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) FROM workbench.cases WHERE assignee = $1 AND status <> 'decided'",
    )
    .bind(user)
    .fetch_one(pool)
    .await
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct CaseDocument {
    pub document_id: Uuid,
    pub role: String,
    pub filename: Option<String>,
    pub added_by: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub added_at: OffsetDateTime,
}

pub async fn documents(pool: &PgPool, case_id: Uuid) -> sqlx::Result<Vec<CaseDocument>> {
    sqlx::query_as(
        "SELECT document_id, role, filename, added_by, added_at FROM workbench.case_documents
         WHERE case_id = $1 ORDER BY role = 'primary' DESC, added_at",
    )
    .bind(case_id)
    .fetch_all(pool)
    .await
}

pub async fn add_document(
    conn: &mut PgConnection,
    case_id: Uuid,
    document_id: Uuid,
    role: &str,
    filename: Option<&str>,
    actor: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO workbench.case_documents (case_id, document_id, role, filename, added_by)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(case_id)
    .bind(document_id)
    .bind(role)
    .bind(filename)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    touch(conn, case_id).await
}

pub async fn remove_document(
    conn: &mut PgConnection,
    case_id: Uuid,
    document_id: Uuid,
) -> sqlx::Result<bool> {
    let n = sqlx::query(
        "DELETE FROM workbench.case_documents WHERE case_id = $1 AND document_id = $2 AND role <> 'primary'",
    )
    .bind(case_id)
    .bind(document_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if n > 0 {
        touch(conn, case_id).await?;
    }
    Ok(n > 0)
}

/// Makes `document_id`, already a case document, the primary; the previous primary becomes
/// `supporting`.
pub async fn set_primary(conn: &mut PgConnection, id: Uuid, document_id: Uuid) -> sqlx::Result<()> {
    // Demote first: one primary per case is enforced row by row.
    sqlx::query(
        "UPDATE workbench.case_documents SET role = 'supporting' WHERE case_id = $1 AND role = 'primary'",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE workbench.case_documents SET role = 'primary' WHERE case_id = $1 AND document_id = $2",
    )
    .bind(id)
    .bind(document_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE workbench.cases SET primary_document_id = $2, revision = revision + 1, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(document_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Marks projected state changed.
async fn touch(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE workbench.cases SET revision = revision + 1, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn set_facts(
    conn: &mut PgConnection,
    id: Uuid,
    facts: &BTreeMap<String, String>,
    overridden: &[String],
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE workbench.cases SET facts = $2, overridden = $3, revision = revision + 1, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(Json(facts))
    .bind(overridden)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn set_form_type(conn: &mut PgConnection, id: Uuid, form_type: &str) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE workbench.cases SET form_type = $2, revision = revision + 1, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(form_type)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn set_received_date(conn: &mut PgConnection, id: Uuid, date: &str) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE workbench.cases SET received_date = $2::date, revision = revision + 1, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(date)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn set_assignee(
    conn: &mut PgConnection,
    id: Uuid,
    assignee: Option<&str>,
    status: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE workbench.cases
         SET assignee = $2, assigned_at = CASE WHEN $2::text IS NULL THEN NULL ELSE now() END, status = $3,
             revision = revision + 1, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(assignee)
    .bind(status)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn set_status(
    conn: &mut PgConnection,
    id: Uuid,
    status: &str,
    response_due: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE workbench.cases SET status = $2, response_due = $3::date, revision = revision + 1, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(response_due)
    .execute(conn)
    .await?;
    Ok(())
}

/// Working assessments are not projected; no revision bump.
pub async fn set_criteria(
    conn: &mut PgConnection,
    id: Uuid,
    criteria: &BTreeMap<String, Assessment>,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE workbench.cases SET criteria = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(Json(criteria))
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn decide(
    conn: &mut PgConnection,
    id: Uuid,
    outcome: &str,
    by: &str,
    artifact_id: Uuid,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE workbench.cases
         SET status = 'decided', decision = $2, decided_by = $3, decided_at = now(), decision_artifact_id = $4,
             response_due = NULL, revision = revision + 1, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(outcome)
    .bind(by)
    .bind(artifact_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Clears the decision; the decision artifact stays in storage and is referenced by the next one.
pub async fn reopen(conn: &mut PgConnection, id: Uuid, assignee: &str) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE workbench.cases
         SET status = 'in_review', assignee = $2, assigned_at = now(), decision = NULL, decided_by = NULL,
             decided_at = NULL, decision_artifact_id = NULL, revision = revision + 1, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(assignee)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn unindexed(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT id FROM workbench.cases WHERE indexed_revision < revision ORDER BY updated_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn mark_indexed(
    pool: &PgPool,
    id: Uuid,
    revision: i32,
    artifact_id: Uuid,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE workbench.cases SET indexed_revision = GREATEST(indexed_revision, $2),
             case_artifact_id = CASE WHEN $2 >= indexed_revision THEN $3 ELSE case_artifact_id END
         WHERE id = $1",
    )
    .bind(id)
    .bind(revision)
    .bind(artifact_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Notice {
    pub id: Uuid,
    pub kind: String,
    #[schema(value_type = Vec<String>)]
    pub items: Json<Vec<String>>,
    pub message: Option<String>,
    pub issued_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub issued_at: OffsetDateTime,
    /// `YYYY-MM-DD`.
    pub due_date: String,
    pub artifact_id: Uuid,
    #[serde(with = "time::serde::rfc3339::option")]
    pub responded_at: Option<OffsetDateTime>,
    pub response_note: Option<String>,
    pub recorded_by: Option<String>,
    /// `YYYY-MM-DD`; due date at issue, set once the notice is extended.
    pub original_due_date: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub rescinded_at: Option<OffsetDateTime>,
    pub rescinded_by: Option<String>,
    pub rescind_reason: Option<String>,
}

const NOTICE_COLS: &str = "id, kind, items, message, issued_by, issued_at, due_date::text AS due_date, artifact_id, \
                           responded_at, response_note, recorded_by, original_due_date::text AS original_due_date, \
                           rescinded_at, rescinded_by, rescind_reason";

pub struct NewNotice<'a> {
    pub id: Uuid,
    pub case_id: Uuid,
    pub kind: &'a str,
    pub items: &'a [String],
    pub message: Option<&'a str>,
    pub issued_by: &'a str,
    pub due_date: &'a str,
    pub artifact_id: Uuid,
}

pub async fn insert_notice(conn: &mut PgConnection, n: &NewNotice<'_>) -> sqlx::Result<Notice> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO workbench.case_notices (id, case_id, kind, items, message, issued_by, due_date, artifact_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7::date, $8) RETURNING {NOTICE_COLS}"
    )))
    .bind(n.id)
    .bind(n.case_id)
    .bind(n.kind)
    .bind(Json(n.items))
    .bind(n.message)
    .bind(n.issued_by)
    .bind(n.due_date)
    .bind(n.artifact_id)
    .fetch_one(conn)
    .await
}

pub async fn notices(pool: &PgPool, case_id: Uuid) -> sqlx::Result<Vec<Notice>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NOTICE_COLS} FROM workbench.case_notices WHERE case_id = $1 ORDER BY issued_at"
    )))
    .bind(case_id)
    .fetch_all(pool)
    .await
}

pub async fn open_notice(conn: &mut PgConnection, case_id: Uuid) -> sqlx::Result<Option<Notice>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NOTICE_COLS} FROM workbench.case_notices
         WHERE case_id = $1 AND responded_at IS NULL AND rescinded_at IS NULL
         ORDER BY issued_at DESC LIMIT 1"
    )))
    .bind(case_id)
    .fetch_optional(conn)
    .await
}

pub async fn respond_notice(
    conn: &mut PgConnection,
    id: Uuid,
    note: Option<&str>,
    by: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE workbench.case_notices SET responded_at = now(), response_note = $2, recorded_by = $3
         WHERE id = $1",
    )
    .bind(id)
    .bind(note)
    .bind(by)
    .execute(conn)
    .await?;
    Ok(())
}

/// Moves the due date; the first extension keeps the issued due date in `original_due_date`.
pub async fn extend_notice(conn: &mut PgConnection, id: Uuid, due_date: &str) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE workbench.case_notices
         SET original_due_date = COALESCE(original_due_date, due_date), due_date = $2::date
         WHERE id = $1",
    )
    .bind(id)
    .bind(due_date)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn rescind_notice(
    conn: &mut PgConnection,
    id: Uuid,
    reason: &str,
    by: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE workbench.case_notices SET rescinded_at = now(), rescind_reason = $2, rescinded_by = $3
         WHERE id = $1",
    )
    .bind(id)
    .bind(reason)
    .bind(by)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn log(
    conn: &mut PgConnection,
    case_id: Uuid,
    actor: Option<&str>,
    action: &str,
    details: Value,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO workbench.case_events (case_id, actor, action, details) VALUES ($1, $2, $3, $4)",
    )
    .bind(case_id)
    .bind(actor)
    .bind(action)
    .bind(Json(details))
    .execute(conn)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct CaseEvent {
    pub id: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub actor: Option<String>,
    pub action: String,
    #[schema(value_type = Object)]
    pub details: Value,
}

pub async fn history(pool: &PgPool, case_id: Uuid) -> sqlx::Result<Vec<CaseEvent>> {
    sqlx::query_as(
        "SELECT id, at, actor, action, details FROM workbench.case_events WHERE case_id = $1 ORDER BY id",
    )
    .bind(case_id)
    .fetch_all(pool)
    .await
}

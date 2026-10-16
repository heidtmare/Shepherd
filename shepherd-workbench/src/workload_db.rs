//! Workload tables in the `workbench` schema: workers, skills, assignment rules, requests and
//! exclusions. Every transaction that changes an assignment takes [`lock_assignments`] first, so
//! capacity checks and assignments are serialized.

use serde::Serialize;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::workload::{Candidate, RuleSettings, WorkerSettings};

/// `pg_advisory_xact_lock` key for assignment changes.
const ASSIGNMENT_LOCK: i64 = 0x5348_4550_4153_4731; // "SHEPASG1"

/// Serializes assignment changes until the transaction ends. Take it before locking case rows.
pub async fn lock_assignments(conn: &mut PgConnection) -> sqlx::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ASSIGNMENT_LOCK)
        .execute(conn)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Worker {
    pub worker: String,
    pub max_cases: i32,
    pub auto_assign: bool,
    pub skills: Vec<String>,
    /// Undecided cases assigned.
    pub load: i64,
    /// Most recent assignment, of any case.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_assigned_at: Option<OffsetDateTime>,
    pub pending_requests: i64,
    pub updated_by: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

const WORKER_COLS: &str = "w.worker, w.max_cases, w.auto_assign, w.updated_by, w.created_at, w.updated_at, \
     ARRAY(SELECT s.form_type FROM workbench.worker_skills s WHERE s.worker = w.worker ORDER BY s.form_type) AS skills, \
     (SELECT count(*) FROM workbench.cases c WHERE c.assignee = w.worker AND c.status <> 'decided') AS load, \
     (SELECT max(c.assigned_at) FROM workbench.cases c WHERE c.assignee = w.worker) AS last_assigned_at, \
     (SELECT count(*) FROM workbench.workload_requests q WHERE q.worker = w.worker AND q.status = 'pending') AS pending_requests";

pub async fn workers(pool: &PgPool) -> sqlx::Result<Vec<Worker>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {WORKER_COLS} FROM workbench.workers w ORDER BY w.worker"
    )))
    .fetch_all(pool)
    .await
}

pub async fn worker(conn: &mut PgConnection, name: &str) -> sqlx::Result<Option<Worker>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {WORKER_COLS} FROM workbench.workers w WHERE w.worker = $1"
    )))
    .bind(name)
    .fetch_optional(conn)
    .await
}

/// Creates or replaces a worker's settings and skills.
pub async fn upsert_worker(
    conn: &mut PgConnection,
    name: &str,
    s: &WorkerSettings,
    actor: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO workbench.workers (worker, max_cases, auto_assign, updated_by) VALUES ($1, $2, $3, $4)
         ON CONFLICT (worker) DO UPDATE
         SET max_cases = EXCLUDED.max_cases, auto_assign = EXCLUDED.auto_assign,
             updated_by = EXCLUDED.updated_by, updated_at = now()",
    )
    .bind(name)
    .bind(s.max_cases)
    .bind(s.auto_assign)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM workbench.worker_skills WHERE worker = $1")
        .bind(name)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO workbench.worker_skills (worker, form_type) SELECT $1, unnest($2::text[])",
    )
    .bind(name)
    .bind(&s.skills)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn delete_worker(conn: &mut PgConnection, name: &str) -> sqlx::Result<bool> {
    Ok(
        sqlx::query("DELETE FROM workbench.workers WHERE worker = $1")
            .bind(name)
            .execute(conn)
            .await?
            .rows_affected()
            > 0,
    )
}

/// Undecided cases assigned to `worker`, other than `except`.
pub async fn load(
    conn: &mut PgConnection,
    worker: &str,
    except: Option<Uuid>,
) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) FROM workbench.cases
         WHERE assignee = $1 AND status <> 'decided' AND ($2::uuid IS NULL OR id <> $2)",
    )
    .bind(worker)
    .bind(except)
    .fetch_one(conn)
    .await
}

/// Form types of the worker's undecided cases, with counts.
pub async fn held_forms(conn: &mut PgConnection, worker: &str) -> sqlx::Result<Vec<(String, i64)>> {
    sqlx::query_as(
        "SELECT form_type, count(*) FROM workbench.cases
         WHERE assignee = $1 AND status <> 'decided' GROUP BY form_type ORDER BY form_type",
    )
    .bind(worker)
    .fetch_all(conn)
    .await
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Rule {
    pub id: Uuid,
    pub name: String,
    pub form_type: String,
    pub workers: Vec<String>,
    pub priority: i32,
    pub enabled: bool,
    pub created_by: String,
    pub updated_by: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

const RULE_COLS: &str = "id, name, form_type, workers, priority, enabled, created_by, updated_by, created_at, updated_at";

pub async fn rules(pool: &PgPool) -> sqlx::Result<Vec<Rule>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {RULE_COLS} FROM workbench.assignment_rules ORDER BY form_type, priority, created_at"
    )))
    .fetch_all(pool)
    .await
}

/// Enabled rules for a form type, in evaluation order.
pub async fn matching_rules(conn: &mut PgConnection, form_type: &str) -> sqlx::Result<Vec<Rule>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {RULE_COLS} FROM workbench.assignment_rules
         WHERE enabled AND form_type = $1 ORDER BY priority, created_at"
    )))
    .bind(form_type)
    .fetch_all(conn)
    .await
}

pub async fn insert_rule(
    conn: &mut PgConnection,
    r: &RuleSettings,
    actor: &str,
) -> sqlx::Result<Rule> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO workbench.assignment_rules (id, name, form_type, workers, priority, enabled, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING {RULE_COLS}"
    )))
    .bind(Uuid::now_v7())
    .bind(&r.name)
    .bind(&r.form_type)
    .bind(&r.workers)
    .bind(r.priority)
    .bind(r.enabled)
    .bind(actor)
    .fetch_one(conn)
    .await
}

pub async fn update_rule(
    conn: &mut PgConnection,
    id: Uuid,
    r: &RuleSettings,
    actor: &str,
) -> sqlx::Result<Option<Rule>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE workbench.assignment_rules
         SET name = $2, form_type = $3, workers = $4, priority = $5, enabled = $6, updated_by = $7, updated_at = now()
         WHERE id = $1 RETURNING {RULE_COLS}"
    )))
    .bind(id)
    .bind(&r.name)
    .bind(&r.form_type)
    .bind(&r.workers)
    .bind(r.priority)
    .bind(r.enabled)
    .bind(actor)
    .fetch_optional(conn)
    .await
}

pub async fn delete_rule(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<bool> {
    Ok(
        sqlx::query("DELETE FROM workbench.assignment_rules WHERE id = $1")
            .bind(id)
            .execute(conn)
            .await?
            .rows_affected()
            > 0,
    )
}

/// Workers a rule may give `case_id` to: auto-assigning, skilled for `form_type`, in `pool`
/// (empty = any) and not excluded from the case. Capacity is left to [`crate::workload::pick`].
pub async fn candidates(
    conn: &mut PgConnection,
    case_id: Uuid,
    form_type: &str,
    pool: &[String],
) -> sqlx::Result<Vec<Candidate>> {
    sqlx::query_as(
        "SELECT w.worker, w.max_cases,
             (SELECT count(*) FROM workbench.cases c WHERE c.assignee = w.worker AND c.status <> 'decided') AS load,
             (SELECT max(c.assigned_at) FROM workbench.cases c WHERE c.assignee = w.worker) AS last_assigned_at
         FROM workbench.workers w
         JOIN workbench.worker_skills s ON s.worker = w.worker AND s.form_type = $2
         WHERE w.auto_assign
           AND (cardinality($3::text[]) = 0 OR w.worker = ANY($3))
           AND NOT EXISTS (SELECT 1 FROM workbench.assignment_exclusions x
                           WHERE x.case_id = $1 AND x.worker = w.worker)",
    )
    .bind(case_id)
    .bind(form_type)
    .bind(pool)
    .fetch_all(conn)
    .await
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BacklogCase {
    pub case_id: Uuid,
    pub rule_id: Uuid,
}

/// Unassigned open cases `worker` may receive from a rule, best first (rule priority, then oldest
/// received). Rows are locked; cases locked by others are skipped.
pub async fn backlog_for(
    conn: &mut PgConnection,
    worker: &str,
    limit: i64,
) -> sqlx::Result<Vec<BacklogCase>> {
    sqlx::query_as(
        "SELECT c.id AS case_id, r.id AS rule_id
         FROM workbench.cases c
         CROSS JOIN LATERAL (
             SELECT r.id, r.priority FROM workbench.assignment_rules r
             WHERE r.enabled AND r.form_type = c.form_type AND (cardinality(r.workers) = 0 OR $1 = ANY(r.workers))
             ORDER BY r.priority, r.created_at LIMIT 1) r
         WHERE c.status = 'open' AND c.assignee IS NULL
           AND EXISTS (SELECT 1 FROM workbench.worker_skills s WHERE s.worker = $1 AND s.form_type = c.form_type)
           AND NOT EXISTS (SELECT 1 FROM workbench.assignment_exclusions x WHERE x.case_id = c.id AND x.worker = $1)
         ORDER BY r.priority, c.received_date, c.created_at
         LIMIT $2
         FOR UPDATE OF c SKIP LOCKED",
    )
    .bind(worker)
    .bind(limit)
    .fetch_all(conn)
    .await
}

/// Unassigned open cases with an enabled rule for their form type, oldest received first.
pub async fn backlog(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT c.id FROM workbench.cases c
         WHERE c.status = 'open' AND c.assignee IS NULL
           AND EXISTS (SELECT 1 FROM workbench.assignment_rules r WHERE r.enabled AND r.form_type = c.form_type)
         ORDER BY c.received_date, c.created_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn exclude(conn: &mut PgConnection, case_id: Uuid, worker: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO workbench.assignment_exclusions (case_id, worker) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(case_id)
    .bind(worker)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn unexclude(conn: &mut PgConnection, case_id: Uuid, worker: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM workbench.assignment_exclusions WHERE case_id = $1 AND worker = $2")
        .bind(case_id)
        .bind(worker)
        .execute(conn)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Request {
    pub id: Uuid,
    pub case_id: Uuid,
    pub receipt_number: String,
    pub form_type: String,
    /// Current assignee of the case.
    pub assignee: Option<String>,
    pub case_status: String,
    pub worker: String,
    pub kind: String,
    pub reason: Option<String>,
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub resolved_by: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub resolved_at: Option<OffsetDateTime>,
    pub resolution_note: Option<String>,
}

const REQUEST_COLS: &str = "q.id, q.case_id, c.receipt_number, c.form_type, c.assignee, c.status AS case_status, \
     q.worker, q.kind, q.reason, q.status, q.created_at, q.resolved_by, q.resolved_at, q.resolution_note";

pub struct NewRequest<'a> {
    pub case_id: Uuid,
    pub worker: &'a str,
    pub kind: &'a str,
    pub reason: Option<&'a str>,
}

/// Inserts a pending request; `None` when the worker already has one of this kind for the case.
pub async fn insert_request(
    conn: &mut PgConnection,
    n: &NewRequest<'_>,
) -> sqlx::Result<Option<Uuid>> {
    sqlx::query_scalar(
        "INSERT INTO workbench.workload_requests (id, case_id, worker, kind, reason, status)
         VALUES ($1, $2, $3, $4, $5, 'pending')
         ON CONFLICT (case_id, worker, kind) WHERE status = 'pending' DO NOTHING RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(n.case_id)
    .bind(n.worker)
    .bind(n.kind)
    .bind(n.reason)
    .fetch_optional(conn)
    .await
}

pub async fn pending_request(
    conn: &mut PgConnection,
    case_id: Uuid,
    worker: &str,
    kind: &str,
) -> sqlx::Result<Option<Uuid>> {
    sqlx::query_scalar(
        "SELECT id FROM workbench.workload_requests
         WHERE case_id = $1 AND worker = $2 AND kind = $3 AND status = 'pending'",
    )
    .bind(case_id)
    .bind(worker)
    .bind(kind)
    .fetch_optional(conn)
    .await
}

pub async fn request(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Option<Request>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {REQUEST_COLS} FROM workbench.workload_requests q
         JOIN workbench.cases c ON c.id = q.case_id WHERE q.id = $1"
    )))
    .bind(id)
    .fetch_optional(conn)
    .await
}

pub struct RequestFilter<'a> {
    /// Empty = all.
    pub statuses: &'a [String],
    pub worker: Option<&'a str>,
    pub case_id: Option<Uuid>,
    pub limit: i64,
}

/// Oldest first while pending; resolved newest first.
pub async fn requests(pool: &PgPool, f: &RequestFilter<'_>) -> sqlx::Result<Vec<Request>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {REQUEST_COLS} FROM workbench.workload_requests q
         JOIN workbench.cases c ON c.id = q.case_id
         WHERE (cardinality($1::text[]) = 0 OR q.status = ANY($1))
           AND ($2::text IS NULL OR q.worker = $2)
           AND ($3::uuid IS NULL OR q.case_id = $3)
         ORDER BY q.status <> 'pending', CASE WHEN q.status = 'pending' THEN q.created_at END,
                  q.resolved_at DESC NULLS LAST
         LIMIT $4"
    )))
    .bind(f.statuses)
    .bind(f.worker)
    .bind(f.case_id)
    .bind(f.limit)
    .fetch_all(pool)
    .await
}

/// Resolves a pending request. Returns false when it was no longer pending.
pub async fn resolve_request(
    conn: &mut PgConnection,
    id: Uuid,
    status: &str,
    by: Option<&str>,
    note: Option<&str>,
) -> sqlx::Result<bool> {
    Ok(sqlx::query(
        "UPDATE workbench.workload_requests
         SET status = $2, resolved_by = $3, resolved_at = now(), resolution_note = $4
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .bind(status)
    .bind(by)
    .bind(note)
    .execute(conn)
    .await?
    .rows_affected()
        > 0)
}

/// Cancels the case's pending requests, e.g. when it is decided.
pub async fn cancel_requests(
    conn: &mut PgConnection,
    case_id: Uuid,
    note: &str,
) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        "UPDATE workbench.workload_requests
         SET status = 'cancelled', resolved_at = now(), resolution_note = $2
         WHERE case_id = $1 AND status = 'pending'",
    )
    .bind(case_id)
    .bind(note)
    .execute(conn)
    .await?
    .rows_affected())
}

pub async fn cancel_worker_requests(
    conn: &mut PgConnection,
    worker: &str,
    note: &str,
) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        "UPDATE workbench.workload_requests
         SET status = 'cancelled', resolved_at = now(), resolution_note = $2
         WHERE worker = $1 AND status = 'pending'",
    )
    .bind(worker)
    .bind(note)
    .execute(conn)
    .await?
    .rows_affected())
}

//! Postgres access. All tables live in the `workbench` schema.

use serde::Serialize;
use serde_json::Value;
use sqlx::{PgConnection, PgPool, postgres::PgPoolOptions, types::Json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

pub async fn connect(url: &str, max: u32) -> anyhow::Result<PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(max)
        .connect(url)
        .await?)
}

pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    let mut m = sqlx::migrate!("./migrations");
    m.dangerous_set_table_name("workbench._sqlx_migrations");
    m.run(pool).await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Task {
    pub id: Uuid,
    pub document_id: Uuid,
    pub filename: Option<String>,
    pub form_type: Option<String>,
    pub kind: String,
    pub status: String,
    pub source_artifact_id: Uuid,
    pub form_artifact_id: Option<Uuid>,
    pub flagged_fields: i32,
    pub total_fields: i32,
    pub assignee: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub claimed_at: Option<OffsetDateTime>,
    pub receipt_artifact_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
}

const COLS: &str = "id, document_id, filename, form_type, kind, status, source_artifact_id, form_artifact_id, \
                    flagged_fields, total_fields, assignee, claimed_at, receipt_artifact_id, created_at, updated_at, completed_at";

pub struct NewTask<'a> {
    pub document_id: Uuid,
    pub filename: Option<&'a str>,
    pub form_type: Option<&'a str>,
    pub kind: &'a str,
    pub source_artifact_id: Uuid,
    pub form_artifact_id: Option<Uuid>,
    pub flagged_fields: i32,
    pub total_fields: i32,
}

/// Creates a task, or refreshes the document's active task with the newer recognition.
/// Returns the task and whether it was newly created.
pub async fn upsert_active(conn: &mut PgConnection, t: &NewTask<'_>) -> sqlx::Result<(Task, bool)> {
    let existing: Option<Task> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM workbench.tasks WHERE document_id = $1 AND status IN ('open', 'in_progress') FOR UPDATE"
    )))
    .bind(t.document_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(e) = existing {
        let task = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE workbench.tasks SET form_artifact_id = $2, form_type = COALESCE($3, form_type),
                 flagged_fields = $4, total_fields = $5, source_artifact_id = $6, updated_at = now()
             WHERE id = $1 RETURNING {COLS}"
        )))
        .bind(e.id)
        .bind(t.form_artifact_id)
        .bind(t.form_type)
        .bind(t.flagged_fields)
        .bind(t.total_fields)
        .bind(t.source_artifact_id)
        .fetch_one(&mut *conn)
        .await?;
        return Ok((task, false));
    }
    let task = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO workbench.tasks (id, document_id, filename, form_type, kind, status, source_artifact_id,
                                         form_artifact_id, flagged_fields, total_fields)
         VALUES ($1, $2, $3, $4, $5, 'open', $6, $7, $8, $9) RETURNING {COLS}"
    )))
    .bind(Uuid::now_v7())
    .bind(t.document_id)
    .bind(t.filename)
    .bind(t.form_type)
    .bind(t.kind)
    .bind(t.source_artifact_id)
    .bind(t.form_artifact_id)
    .bind(t.flagged_fields)
    .bind(t.total_fields)
    .fetch_one(&mut *conn)
    .await?;
    Ok((task, true))
}

/// Records an amendment of stored form values as a completed task, so every receipt has a task.
pub async fn insert_amendment(
    conn: &mut PgConnection,
    id: Uuid,
    t: &NewTask<'_>,
    actor: &str,
    receipt_artifact_id: Uuid,
) -> sqlx::Result<Task> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO workbench.tasks (id, document_id, filename, form_type, kind, status, source_artifact_id,
                                         form_artifact_id, flagged_fields, total_fields, assignee, claimed_at,
                                         receipt_artifact_id, completed_at)
         VALUES ($1, $2, $3, $4, 'amend', 'completed', $5, $6, 0, $7, $8, now(), $9, now()) RETURNING {COLS}"
    )))
    .bind(id)
    .bind(t.document_id)
    .bind(t.filename)
    .bind(t.form_type)
    .bind(t.source_artifact_id)
    .bind(t.form_artifact_id)
    .bind(t.total_fields)
    .bind(actor)
    .bind(receipt_artifact_id)
    .fetch_one(conn)
    .await
}

pub async fn active_for_document(pool: &PgPool, document_id: Uuid) -> sqlx::Result<Option<Task>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM workbench.tasks WHERE document_id = $1 AND status IN ('open', 'in_progress')"
    )))
    .bind(document_id)
    .fetch_optional(pool)
    .await
}

pub async fn tasks_for_document(pool: &PgPool, document_id: Uuid) -> sqlx::Result<Vec<Task>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM workbench.tasks WHERE document_id = $1 ORDER BY created_at DESC"
    )))
    .bind(document_id)
    .fetch_all(pool)
    .await
}

pub async fn get(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<Task>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM workbench.tasks WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn lock(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Option<Task>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM workbench.tasks WHERE id = $1 FOR UPDATE"
    )))
    .bind(id)
    .fetch_optional(conn)
    .await
}

/// Queue page: oldest first. `statuses` empty = all.
pub async fn list(
    pool: &PgPool,
    statuses: &[String],
    form_type: Option<&str>,
    limit: i64,
    offset: i64,
) -> sqlx::Result<Vec<Task>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM workbench.tasks
         WHERE (cardinality($1::text[]) = 0 OR status = ANY($1)) AND ($2::text IS NULL OR form_type = $2)
         ORDER BY CASE WHEN status IN ('completed', 'cancelled') THEN 1 ELSE 0 END,
                  CASE WHEN status IN ('completed', 'cancelled') THEN completed_at END DESC NULLS LAST,
                  created_at
         LIMIT $3 OFFSET $4"
    )))
    .bind(statuses)
    .bind(form_type)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct StatusCount {
    pub status: String,
    pub count: i64,
}

pub async fn counts(pool: &PgPool) -> sqlx::Result<Vec<StatusCount>> {
    sqlx::query_as(
        "SELECT status, count(*) AS count FROM workbench.tasks GROUP BY status ORDER BY status",
    )
    .fetch_all(pool)
    .await
}

pub async fn set_claim(
    conn: &mut PgConnection,
    id: Uuid,
    assignee: Option<&str>,
) -> sqlx::Result<Task> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE workbench.tasks
         SET assignee = $2, claimed_at = CASE WHEN $2::text IS NULL THEN NULL ELSE now() END,
             status = CASE WHEN $2::text IS NULL THEN 'open' ELSE 'in_progress' END, updated_at = now()
         WHERE id = $1 RETURNING {COLS}"
    )))
    .bind(id)
    .bind(assignee)
    .fetch_one(conn)
    .await
}

pub async fn set_form_type(
    conn: &mut PgConnection,
    id: Uuid,
    form_type: &str,
) -> sqlx::Result<Task> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE workbench.tasks SET form_type = $2, updated_at = now() WHERE id = $1 RETURNING {COLS}"
    )))
    .bind(id)
    .bind(form_type)
    .fetch_one(conn)
    .await
}

pub async fn complete(
    conn: &mut PgConnection,
    id: Uuid,
    receipt_artifact_id: Uuid,
) -> sqlx::Result<Task> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE workbench.tasks SET status = 'completed', receipt_artifact_id = $2, completed_at = now(), updated_at = now()
         WHERE id = $1 RETURNING {COLS}"
    )))
    .bind(id)
    .bind(receipt_artifact_id)
    .fetch_one(conn)
    .await
}

pub async fn cancel(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Task> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE workbench.tasks SET status = 'cancelled', completed_at = now(), updated_at = now(), assignee = NULL
         WHERE id = $1 RETURNING {COLS}"
    )))
    .bind(id)
    .fetch_one(conn)
    .await
}

pub async fn log(
    conn: &mut PgConnection,
    task_id: Uuid,
    actor: Option<&str>,
    action: &str,
    details: Value,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO workbench.task_events (task_id, actor, action, details) VALUES ($1, $2, $3, $4)")
        .bind(task_id)
        .bind(actor)
        .bind(action)
        .bind(Json(details))
        .execute(conn)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct TaskEvent {
    pub id: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub actor: Option<String>,
    pub action: String,
    #[schema(value_type = Object)]
    pub details: Value,
}

pub async fn history(pool: &PgPool, task_id: Uuid) -> sqlx::Result<Vec<TaskEvent>> {
    sqlx::query_as("SELECT id, at, actor, action, details FROM workbench.task_events WHERE task_id = $1 ORDER BY id")
        .bind(task_id)
        .fetch_all(pool)
        .await
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Draft {
    pub mode: String,
    #[schema(value_type = Object)]
    pub values: Value,
    pub updated_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

pub async fn draft(pool: &PgPool, task_id: Uuid) -> sqlx::Result<Option<Draft>> {
    sqlx::query_as(
        "SELECT mode, \"values\", updated_by, updated_at FROM workbench.drafts WHERE task_id = $1",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await
}

pub async fn save_draft(
    pool: &PgPool,
    task_id: Uuid,
    mode: &str,
    values: &Value,
    user: &str,
) -> sqlx::Result<Draft> {
    sqlx::query_as(
        "INSERT INTO workbench.drafts (task_id, mode, \"values\", updated_by) VALUES ($1, $2, $3, $4)
         ON CONFLICT (task_id) DO UPDATE SET mode = EXCLUDED.mode, \"values\" = EXCLUDED.\"values\",
             updated_by = EXCLUDED.updated_by, updated_at = now()
         RETURNING mode, \"values\", updated_by, updated_at",
    )
    .bind(task_id)
    .bind(mode)
    .bind(Json(values))
    .bind(user)
    .fetch_one(pool)
    .await
}

pub async fn delete_draft(conn: &mut PgConnection, task_id: Uuid) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM workbench.drafts WHERE task_id = $1")
        .bind(task_id)
        .execute(conn)
        .await?;
    Ok(())
}

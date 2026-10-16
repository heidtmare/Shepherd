//! Postgres access. All tables live in the `storage` schema.

use serde::Serialize;
use serde_json::Value;
use shepherd_common::events::Envelope;
use sqlx::{PgConnection, PgPool, postgres::PgPoolOptions, types::Json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

pub async fn connect(url: &str, max_connections: u32) -> anyhow::Result<PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(url)
        .await?)
}

pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    let mut m = sqlx::migrate!("./migrations");
    m.dangerous_set_table_name("storage._sqlx_migrations");
    m.run(pool).await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Document {
    pub id: Uuid,
    pub source_filename: Option<String>,
    pub content_type: String,
    pub source_sha256: String,
    pub size_bytes: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub created_by: Option<String>,
    #[schema(value_type = Object)]
    pub labels: Value,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Artifact {
    pub id: Uuid,
    pub document_id: Uuid,
    pub kind: String,
    pub schema: String,
    pub version: i32,
    pub content_type: String,
    pub sha256: String,
    pub size_bytes: i64,
    pub object_key: String,
    pub produced_by: Option<String>,
    pub parent_artifact_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct AuditEntry {
    pub id: i64,
    pub document_id: Uuid,
    pub artifact_id: Option<Uuid>,
    pub action: String,
    pub actor: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    #[schema(value_type = Object)]
    pub details: Value,
}

pub const SOURCE_SCHEMA: &str = "source";

const ARTIFACT_COLS: &str = "id, document_id, kind, schema, version, content_type, sha256, size_bytes, \
                             object_key, produced_by, parent_artifact_id, created_at";

pub struct NewDocument<'a> {
    pub id: Uuid,
    pub filename: Option<&'a str>,
    pub content_type: &'a str,
    pub sha256: &'a str,
    pub size_bytes: i64,
    pub created_by: Option<&'a str>,
    pub labels: Value,
}

pub async fn insert_document(
    conn: &mut PgConnection,
    d: &NewDocument<'_>,
) -> sqlx::Result<Document> {
    sqlx::query_as(
        "INSERT INTO storage.documents (id, source_filename, content_type, source_sha256, size_bytes, created_by, labels)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id, source_filename, content_type, source_sha256, size_bytes, created_at, created_by, labels",
    )
    .bind(d.id)
    .bind(d.filename)
    .bind(d.content_type)
    .bind(d.sha256)
    .bind(d.size_bytes)
    .bind(d.created_by)
    .bind(Json(&d.labels))
    .fetch_one(conn)
    .await
}

pub struct NewArtifact<'a> {
    pub id: Uuid,
    pub document_id: Uuid,
    pub kind: &'a str,
    pub schema: &'a str,
    pub content_type: &'a str,
    pub sha256: &'a str,
    pub size_bytes: i64,
    pub object_key: &'a str,
    pub produced_by: Option<&'a str>,
    pub parent_artifact_id: Option<Uuid>,
}

/// Inserts the next version for `(document_id, schema)`. Caller must hold the document row lock.
pub async fn insert_artifact(
    conn: &mut PgConnection,
    a: &NewArtifact<'_>,
) -> sqlx::Result<Artifact> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO storage.artifacts
             (id, document_id, kind, schema, version, content_type, sha256, size_bytes, object_key, produced_by, parent_artifact_id)
         SELECT $1, $2, $3, $4, COALESCE(MAX(version), 0) + 1, $5, $6, $7, $8, $9, $10
         FROM storage.artifacts WHERE document_id = $2 AND schema = $4
         RETURNING {ARTIFACT_COLS}"
    )))
    .bind(a.id)
    .bind(a.document_id)
    .bind(a.kind)
    .bind(a.schema)
    .bind(a.content_type)
    .bind(a.sha256)
    .bind(a.size_bytes)
    .bind(a.object_key)
    .bind(a.produced_by)
    .bind(a.parent_artifact_id)
    .fetch_one(conn)
    .await
}

/// Row-locks the document so concurrent artifact writes serialize on version assignment.
pub async fn lock_document(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<bool> {
    Ok(
        sqlx::query("SELECT 1 FROM storage.documents WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(conn)
            .await?
            .is_some(),
    )
}

pub async fn insert_audit(
    conn: &mut PgConnection,
    document_id: Uuid,
    artifact_id: Option<Uuid>,
    action: &str,
    actor: Option<&str>,
    details: Value,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO storage.audit_log (document_id, artifact_id, action, actor, details) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(document_id)
    .bind(artifact_id)
    .bind(action)
    .bind(actor)
    .bind(Json(details))
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn enqueue<T: Serialize>(
    conn: &mut PgConnection,
    subject: &str,
    ev: &Envelope<T>,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO storage.outbox (subject, payload) VALUES ($1, $2)")
        .bind(subject)
        .bind(Json(ev))
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn get_document(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<Document>> {
    sqlx::query_as(
        "SELECT id, source_filename, content_type, source_sha256, size_bytes, created_at, created_by, labels
         FROM storage.documents WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Keyset page ordered by id (UUIDv7, so roughly by creation time).
/// The first page uses a separate statement: `$1 IS NULL OR id > $1` makes a cached generic plan
/// scan the pkey from the start on every page.
pub async fn list_documents(
    pool: &PgPool,
    after: Option<Uuid>,
    limit: i64,
    desc: bool,
) -> sqlx::Result<Vec<Document>> {
    const COLS: &str = "SELECT id, source_filename, content_type, source_sha256, size_bytes, created_at, created_by, labels
                        FROM storage.documents";
    let sql = match (after.is_some(), desc) {
        (false, false) => format!("{COLS} ORDER BY id LIMIT $1"),
        (false, true) => format!("{COLS} ORDER BY id DESC LIMIT $1"),
        (true, false) => format!("{COLS} WHERE id > $2 ORDER BY id LIMIT $1"),
        (true, true) => format!("{COLS} WHERE id < $2 ORDER BY id DESC LIMIT $1"),
    };
    let mut q = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(limit);
    if let Some(a) = after {
        q = q.bind(a);
    }
    q.fetch_all(pool).await
}

pub async fn list_artifacts(pool: &PgPool, document_id: Uuid) -> sqlx::Result<Vec<Artifact>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ARTIFACT_COLS} FROM storage.artifacts WHERE document_id = $1 ORDER BY schema, version"
    )))
    .bind(document_id)
    .fetch_all(pool)
    .await
}

pub async fn get_artifact(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<Artifact>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ARTIFACT_COLS} FROM storage.artifacts WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn get_source_artifact(
    pool: &PgPool,
    document_id: Uuid,
) -> sqlx::Result<Option<Artifact>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ARTIFACT_COLS} FROM storage.artifacts
         WHERE document_id = $1 AND kind = 'source' ORDER BY version DESC LIMIT 1"
    )))
    .bind(document_id)
    .fetch_optional(pool)
    .await
}

pub async fn artifact_belongs_to(
    conn: &mut PgConnection,
    artifact_id: Uuid,
    document_id: Uuid,
) -> sqlx::Result<bool> {
    Ok(
        sqlx::query("SELECT 1 FROM storage.artifacts WHERE id = $1 AND document_id = $2")
            .bind(artifact_id)
            .bind(document_id)
            .fetch_optional(conn)
            .await?
            .is_some(),
    )
}

pub async fn list_audit(pool: &PgPool, document_id: Uuid) -> sqlx::Result<Vec<AuditEntry>> {
    sqlx::query_as(
        "SELECT id, document_id, artifact_id, action, actor, at, details
         FROM storage.audit_log WHERE document_id = $1 ORDER BY id",
    )
    .bind(document_id)
    .fetch_all(pool)
    .await
}

//! Postgres access. All tables live in the `notification` schema.

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
    m.dangerous_set_table_name("notification._sqlx_migrations");
    m.run(pool).await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Letter {
    pub id: Uuid,
    pub case_id: Uuid,
    pub receipt_number: String,
    pub document_id: Uuid,
    pub kind: String,
    pub source_id: Uuid,
    pub source_artifact_id: Option<Uuid>,
    /// `held` or `generated`.
    pub status: String,
    pub hold_reason: Option<String>,
    /// Last generation error; cleared when the letter is held for missing data or generated.
    pub last_error: Option<String>,
    pub attempts: i32,
    pub artifact_id: Option<Uuid>,
    pub pdf_artifact_id: Option<Uuid>,
    #[schema(value_type = Option<Object>)]
    pub recipient: Option<Json<Value>>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub generated_at: Option<OffsetDateTime>,
}

const LETTER_COLS: &str = "id, case_id, receipt_number, document_id, kind, source_id, source_artifact_id, status, \
                           hold_reason, last_error, attempts, artifact_id, pdf_artifact_id, recipient, created_at, updated_at, generated_at";

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Delivery {
    pub id: Uuid,
    pub letter_id: Uuid,
    /// `mail` or `email`.
    pub channel: String,
    pub address: String,
    #[serde(skip)]
    pub subject: Option<String>,
    #[serde(skip)]
    pub body: Option<String>,
    /// `queued`, `sent` or `failed`.
    pub status: String,
    pub attempts: i32,
    pub last_error: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub next_attempt_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub sent_at: Option<OffsetDateTime>,
    pub sent_by: Option<String>,
    pub reference: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

const DELIVERY_COLS: &str = "id, letter_id, channel, address, subject, body, status, attempts, last_error, \
                             next_attempt_at, sent_at, sent_by, reference, created_at";

pub struct NewLetter<'a> {
    pub case_id: Uuid,
    pub receipt_number: &'a str,
    pub document_id: Uuid,
    pub kind: &'a str,
    pub source_id: Uuid,
    pub source_artifact_id: Option<Uuid>,
}

/// Records the letter for a triggering event; returns the existing id when already recorded.
pub async fn upsert_letter(pool: &PgPool, l: &NewLetter<'_>) -> sqlx::Result<Uuid> {
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO notification.letters (id, case_id, receipt_number, document_id, kind, source_id, source_artifact_id, status, hold_reason)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'held', 'not yet generated')
         ON CONFLICT (kind, source_id) DO NOTHING RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(l.case_id)
    .bind(l.receipt_number)
    .bind(l.document_id)
    .bind(l.kind)
    .bind(l.source_id)
    .bind(l.source_artifact_id)
    .fetch_optional(pool)
    .await?;
    match inserted {
        Some(id) => Ok(id),
        None => {
            sqlx::query_scalar(
                "SELECT id FROM notification.letters WHERE kind = $1 AND source_id = $2",
            )
            .bind(l.kind)
            .bind(l.source_id)
            .fetch_one(pool)
            .await
        }
    }
}

pub async fn lock_letter(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Option<Letter>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {LETTER_COLS} FROM notification.letters WHERE id = $1 FOR UPDATE"
    )))
    .bind(id)
    .fetch_optional(conn)
    .await
}

pub async fn get_letter(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<Letter>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {LETTER_COLS} FROM notification.letters WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Keeps the letter held with `reason`. Returns whether the reason changed.
pub async fn hold(conn: &mut PgConnection, l: &Letter, reason: &str) -> sqlx::Result<bool> {
    sqlx::query(
        "UPDATE notification.letters SET hold_reason = $2, last_error = NULL, attempts = attempts + 1, updated_at = now()
         WHERE id = $1",
    )
    .bind(l.id)
    .bind(reason)
    .execute(conn)
    .await?;
    Ok(l.hold_reason.as_deref() != Some(reason))
}

pub async fn mark_generated(
    conn: &mut PgConnection,
    id: Uuid,
    artifact_id: Uuid,
    pdf_artifact_id: Uuid,
    recipient: &Value,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE notification.letters SET status = 'generated', hold_reason = NULL, last_error = NULL, attempts = attempts + 1,
                artifact_id = $2, pdf_artifact_id = $3, recipient = $4, generated_at = now(), updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(artifact_id)
    .bind(pdf_artifact_id)
    .bind(Json(recipient))
    .execute(conn)
    .await?;
    Ok(())
}

/// Records a failed generation attempt on a held letter. Bumping `updated_at` moves it to the back
/// of the retry scan.
pub async fn record_error(pool: &PgPool, id: Uuid, error: &str) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE notification.letters SET last_error = $2, attempts = attempts + 1, updated_at = now()
         WHERE id = $1 AND status = 'held'",
    )
    .bind(id)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

pub struct NewDelivery<'a> {
    pub letter_id: Uuid,
    pub channel: &'a str,
    pub address: &'a str,
    pub subject: Option<&'a str>,
    pub body: Option<&'a str>,
}

pub async fn insert_delivery(conn: &mut PgConnection, d: &NewDelivery<'_>) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO notification.deliveries (id, letter_id, channel, address, subject, body, status)
         VALUES ($1, $2, $3, $4, $5, $6, 'queued') ON CONFLICT (letter_id, channel) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(d.letter_id)
    .bind(d.channel)
    .bind(d.address)
    .bind(d.subject)
    .bind(d.body)
    .execute(conn)
    .await?;
    Ok(())
}

/// Held letters, least recently tried first.
pub async fn held(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT id FROM notification.letters WHERE status = 'held' ORDER BY updated_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub struct LetterFilter<'a> {
    pub case_id: Option<Uuid>,
    pub status: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub limit: i64,
}

pub async fn letters(pool: &PgPool, f: &LetterFilter<'_>) -> sqlx::Result<Vec<Letter>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {LETTER_COLS} FROM notification.letters
         WHERE ($1::uuid IS NULL OR case_id = $1) AND ($2::text IS NULL OR status = $2)
           AND ($3::text IS NULL OR kind = $3)
         ORDER BY created_at DESC LIMIT $4"
    )))
    .bind(f.case_id)
    .bind(f.status)
    .bind(f.kind)
    .bind(f.limit)
    .fetch_all(pool)
    .await
}

pub async fn deliveries_for(pool: &PgPool, letter_ids: &[Uuid]) -> sqlx::Result<Vec<Delivery>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {DELIVERY_COLS} FROM notification.deliveries WHERE letter_id = ANY($1) ORDER BY created_at"
    )))
    .bind(letter_ids)
    .fetch_all(pool)
    .await
}

pub struct DeliveryFilter<'a> {
    pub channel: Option<&'a str>,
    pub status: Option<&'a str>,
    pub limit: i64,
}

/// Oldest first, so the mail queue is worked in order.
pub async fn deliveries(pool: &PgPool, f: &DeliveryFilter<'_>) -> sqlx::Result<Vec<Delivery>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {DELIVERY_COLS} FROM notification.deliveries
         WHERE ($1::text IS NULL OR channel = $1) AND ($2::text IS NULL OR status = $2)
         ORDER BY created_at LIMIT $3"
    )))
    .bind(f.channel)
    .bind(f.status)
    .bind(f.limit)
    .fetch_all(pool)
    .await
}

pub async fn lock_delivery(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Option<Delivery>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {DELIVERY_COLS} FROM notification.deliveries WHERE id = $1 FOR UPDATE"
    )))
    .bind(id)
    .fetch_optional(conn)
    .await
}

/// Queued e-mail due for an attempt, locked for this transaction.
pub async fn claim_due_email(conn: &mut PgConnection) -> sqlx::Result<Option<Delivery>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {DELIVERY_COLS} FROM notification.deliveries
         WHERE status = 'queued' AND channel = 'email' AND next_attempt_at <= now()
         ORDER BY next_attempt_at LIMIT 1 FOR UPDATE SKIP LOCKED"
    )))
    .fetch_optional(conn)
    .await
}

pub async fn mark_sent(
    conn: &mut PgConnection,
    id: Uuid,
    sent_by: Option<&str>,
    reference: Option<&str>,
) -> sqlx::Result<Delivery> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE notification.deliveries SET status = 'sent', attempts = attempts + 1, last_error = NULL,
                sent_at = now(), sent_by = $2, reference = $3, updated_at = now()
         WHERE id = $1 RETURNING {DELIVERY_COLS}"
    )))
    .bind(id)
    .bind(sent_by)
    .bind(reference)
    .fetch_one(conn)
    .await
}

/// Records a failed attempt: requeued after `retry_in`, or `failed` when `terminal`.
pub async fn mark_attempt_failed(
    conn: &mut PgConnection,
    id: Uuid,
    error: &str,
    terminal: bool,
    retry_in: time::Duration,
) -> sqlx::Result<Delivery> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE notification.deliveries SET attempts = attempts + 1, last_error = $2,
                status = CASE WHEN $3 THEN 'failed' ELSE status END,
                next_attempt_at = now() + $4, updated_at = now()
         WHERE id = $1 RETURNING {DELIVERY_COLS}"
    )))
    .bind(id)
    .bind(error)
    .bind(terminal)
    .bind(retry_in)
    .fetch_one(conn)
    .await
}

/// Returns a failed delivery to the queue with its attempts reset.
pub async fn requeue(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Delivery> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE notification.deliveries SET status = 'queued', attempts = 0, next_attempt_at = now(), updated_at = now()
         WHERE id = $1 RETURNING {DELIVERY_COLS}"
    )))
    .bind(id)
    .fetch_one(conn)
    .await
}

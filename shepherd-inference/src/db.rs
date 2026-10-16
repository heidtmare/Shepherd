//! Postgres access. All tables live in the `inference` schema.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Value;
use sqlx::{PgConnection, PgPool, postgres::PgPoolOptions, types::Json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::rules::{CaseRef, Party};

pub async fn connect(url: &str, max: u32) -> anyhow::Result<PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(max)
        .connect(url)
        .await?)
}

pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    let mut m = sqlx::migrate!("./migrations");
    m.dangerous_set_table_name("inference._sqlx_migrations");
    m.run(pool).await?;
    Ok(())
}

/// Takes transaction-scoped advisory locks on `keys`. Keys are taken in ascending order, so
/// transactions locking overlapping sets cannot deadlock.
pub async fn lock_keys(conn: &mut PgConnection, keys: &BTreeSet<i64>) -> sqlx::Result<()> {
    let keys: Vec<i64> = keys.iter().copied().collect();
    sqlx::query("SELECT count(pg_advisory_xact_lock(k)) FROM unnest($1::bigint[]) k")
        .bind(keys)
        .execute(conn)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CaseRow {
    pub case_id: Uuid,
    pub receipt_number: String,
    pub form_type: String,
    pub status: String,
    pub document_id: Uuid,
    pub case_artifact_id: Uuid,
    pub case_revision: i32,
    pub findings_revision: i32,
    pub written_revision: i32,
}

impl CaseRow {
    pub fn case_ref(&self) -> CaseRef {
        CaseRef {
            case_id: self.case_id,
            receipt_number: self.receipt_number.clone(),
            form_type: self.form_type.clone(),
            status: self.status.clone(),
        }
    }
}

const CASE_COLS: &str = "case_id, receipt_number, form_type, status, document_id, case_artifact_id, \
                         case_revision, findings_revision, written_revision";

pub async fn case(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Option<CaseRow>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CASE_COLS} FROM inference.cases WHERE case_id = $1"
    )))
    .bind(id)
    .fetch_optional(conn)
    .await
}

/// Party values from a case projection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PartyFacts {
    pub role: &'static str,
    pub family_name: Option<String>,
    pub given_name: Option<String>,
    pub a_number: Option<String>,
    pub date_of_birth: Option<String>,
}

pub struct Snapshot<'a> {
    pub case_id: Uuid,
    pub receipt_number: &'a str,
    pub form_type: &'a str,
    pub status: &'a str,
    pub document_id: Uuid,
    pub case_artifact_id: Uuid,
    pub case_revision: i32,
    pub parties: &'a [PartyFacts],
}

/// Stores the case and replaces its parties. Returns false, changing nothing, when a newer
/// revision is already stored.
pub async fn upsert_case(conn: &mut PgConnection, s: &Snapshot<'_>) -> sqlx::Result<bool> {
    let applied = sqlx::query(
        "INSERT INTO inference.cases (case_id, receipt_number, form_type, status, document_id, case_artifact_id, case_revision)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (case_id) DO UPDATE SET receipt_number = EXCLUDED.receipt_number,
             form_type = EXCLUDED.form_type, status = EXCLUDED.status, document_id = EXCLUDED.document_id,
             -- A new primary document gets the findings artifact.
             findings_revision = inference.cases.findings_revision
                 + CASE WHEN inference.cases.document_id <> EXCLUDED.document_id THEN 1 ELSE 0 END,
             case_artifact_id = EXCLUDED.case_artifact_id, case_revision = EXCLUDED.case_revision,
             updated_at = now()
         WHERE inference.cases.case_revision <= EXCLUDED.case_revision",
    )
    .bind(s.case_id)
    .bind(s.receipt_number)
    .bind(s.form_type)
    .bind(s.status)
    .bind(s.document_id)
    .bind(s.case_artifact_id)
    .bind(s.case_revision)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        > 0;
    if !applied {
        return Ok(false);
    }
    sqlx::query("DELETE FROM inference.parties WHERE case_id = $1")
        .bind(s.case_id)
        .execute(&mut *conn)
        .await?;
    for p in s.parties {
        sqlx::query(
            "INSERT INTO inference.parties (case_id, role, family_name, given_name, a_number, date_of_birth)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(s.case_id)
        .bind(p.role)
        .bind(&p.family_name)
        .bind(&p.given_name)
        .bind(&p.a_number)
        .bind(&p.date_of_birth)
        .execute(&mut *conn)
        .await?;
    }
    Ok(true)
}

const PARTY_COLS: &str = "p.case_id, c.receipt_number, c.form_type, p.role, p.family_name, \
                          p.given_name, p.a_number, p.date_of_birth";

pub async fn parties(conn: &mut PgConnection, case_id: Uuid) -> sqlx::Result<Vec<Party>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PARTY_COLS} FROM inference.parties p JOIN inference.cases c USING (case_id)
         WHERE p.case_id = $1 ORDER BY p.role"
    )))
    .bind(case_id)
    .fetch_all(conn)
    .await
}

/// Parties of other cases holding any of `a_numbers`.
pub async fn related(
    conn: &mut PgConnection,
    case_id: Uuid,
    a_numbers: &[String],
) -> sqlx::Result<Vec<Party>> {
    if a_numbers.is_empty() {
        return Ok(vec![]);
    }
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PARTY_COLS} FROM inference.parties p JOIN inference.cases c USING (case_id)
         WHERE p.a_number = ANY($2) AND p.case_id <> $1 ORDER BY c.receipt_number, p.role"
    )))
    .bind(case_id)
    .bind(a_numbers)
    .fetch_all(conn)
    .await
}

/// A-Numbers held by parties of `case_ids`, sorted.
pub async fn a_numbers(conn: &mut PgConnection, case_ids: &[Uuid]) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT DISTINCT a_number FROM inference.parties
         WHERE case_id = ANY($1) AND a_number IS NOT NULL ORDER BY 1",
    )
    .bind(case_ids)
    .fetch_all(conn)
    .await
}

/// Number of cases holding each of `a_numbers`, other than `exclude`, counted up to `cap`.
pub async fn case_counts(
    conn: &mut PgConnection,
    a_numbers: &[String],
    exclude: Option<Uuid>,
    cap: i64,
) -> sqlx::Result<Vec<(String, i64)>> {
    sqlx::query_as(
        "SELECT k, (SELECT count(*) FROM (
                SELECT 1 FROM inference.parties
                WHERE a_number = k AND ($2::uuid IS NULL OR case_id <> $2)
                GROUP BY case_id LIMIT $3) s)
         FROM unnest($1::text[]) k",
    )
    .bind(a_numbers)
    .bind(exclude)
    .bind(cap)
    .fetch_all(conn)
    .await
}

/// Other cases whose findings may change with `case_id`: those sharing an A-Number, and those
/// with an open finding naming it.
pub async fn affected(
    conn: &mut PgConnection,
    case_id: Uuid,
    a_numbers: &[String],
) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT case_id FROM inference.parties WHERE a_number = ANY($2) AND case_id <> $1
         UNION
         SELECT case_id FROM inference.findings WHERE status = 'open' AND related_cases @> ARRAY[$1]
         ORDER BY 1",
    )
    .bind(case_id)
    .bind(a_numbers)
    .fetch_all(conn)
    .await
}

/// Keyset page of case ids, for full re-evaluation.
pub async fn case_ids_after(
    pool: &PgPool,
    after: Option<Uuid>,
    limit: i64,
) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT case_id FROM inference.cases WHERE ($1::uuid IS NULL OR case_id > $1)
         ORDER BY case_id LIMIT $2",
    )
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Moves a case's findings artifact from its former primary document to `to`. Returns false when
/// the case is unknown or already on another document.
pub async fn move_document(
    pool: &PgPool,
    case_id: Uuid,
    from: Uuid,
    to: Uuid,
) -> sqlx::Result<bool> {
    Ok(sqlx::query(
        "UPDATE inference.cases SET document_id = $3, findings_revision = findings_revision + 1, updated_at = now()
         WHERE case_id = $1 AND document_id = $2",
    )
    .bind(case_id)
    .bind(from)
    .bind(to)
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

pub async fn mark_evaluated(
    conn: &mut PgConnection,
    case_id: Uuid,
    findings_changed: bool,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE inference.cases SET evaluated_at = now(),
             findings_revision = findings_revision + CASE WHEN $2 THEN 1 ELSE 0 END
         WHERE case_id = $1",
    )
    .bind(case_id)
    .bind(findings_changed)
    .execute(conn)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Finding {
    pub id: Uuid,
    pub rule: String,
    pub case_id: Uuid,
    pub key: String,
    /// `low`, `medium` or `high`.
    pub severity: String,
    /// `open` or `resolved`.
    pub status: String,
    pub summary: String,
    #[schema(value_type = Object)]
    pub evidence: Json<Value>,
    pub related_cases: Vec<Uuid>,
    /// `confirmed` or `dismissed`.
    pub review: Option<String>,
    pub reviewed_by: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub reviewed_at: Option<OffsetDateTime>,
    pub review_note: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub raised_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub resolved_at: Option<OffsetDateTime>,
}

const FINDING_COLS: &str = "id, rule, case_id, key, severity, status, summary, evidence, related_cases, review, \
                            reviewed_by, reviewed_at, review_note, raised_at, updated_at, resolved_at";

pub async fn open_findings(conn: &mut PgConnection, case_id: Uuid) -> sqlx::Result<Vec<Finding>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FINDING_COLS} FROM inference.findings WHERE case_id = $1 AND status = 'open'
         ORDER BY raised_at, id FOR UPDATE"
    )))
    .bind(case_id)
    .fetch_all(conn)
    .await
}

pub struct NewFinding<'a> {
    pub rule: &'a str,
    pub case_id: Uuid,
    pub key: &'a str,
    pub severity: &'a str,
    pub summary: &'a str,
    pub evidence: &'a Value,
    pub related_cases: &'a [Uuid],
}

pub async fn insert_finding(conn: &mut PgConnection, f: &NewFinding<'_>) -> sqlx::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO inference.findings (id, rule, case_id, key, severity, status, summary, evidence, related_cases)
         VALUES ($1, $2, $3, $4, $5, 'open', $6, $7, $8)",
    )
    .bind(id)
    .bind(f.rule)
    .bind(f.case_id)
    .bind(f.key)
    .bind(f.severity)
    .bind(f.summary)
    .bind(Json(f.evidence))
    .bind(f.related_cases)
    .execute(conn)
    .await?;
    Ok(id)
}

/// Replaces the evidence of an open finding; a changed set of related cases clears the review.
pub async fn update_finding(
    conn: &mut PgConnection,
    id: Uuid,
    f: &NewFinding<'_>,
    clear_review: bool,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE inference.findings SET severity = $2, summary = $3, evidence = $4, related_cases = $5,
             review = CASE WHEN $6 THEN NULL ELSE review END,
             reviewed_by = CASE WHEN $6 THEN NULL ELSE reviewed_by END,
             reviewed_at = CASE WHEN $6 THEN NULL ELSE reviewed_at END,
             review_note = CASE WHEN $6 THEN NULL ELSE review_note END,
             updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(f.severity)
    .bind(f.summary)
    .bind(Json(f.evidence))
    .bind(f.related_cases)
    .bind(clear_review)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn resolve_finding(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE inference.findings SET status = 'resolved', resolved_at = now(), updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn get_finding(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<Finding>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FINDING_COLS} FROM inference.findings WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn lock_finding(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Option<Finding>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FINDING_COLS} FROM inference.findings WHERE id = $1 FOR UPDATE"
    )))
    .bind(id)
    .fetch_optional(conn)
    .await
}

pub async fn review_finding(
    conn: &mut PgConnection,
    id: Uuid,
    review: Option<&str>,
    by: &str,
    note: Option<&str>,
) -> sqlx::Result<Finding> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE inference.findings SET review = $2, reviewed_by = $3, reviewed_at = now(), review_note = $4,
             updated_at = now()
         WHERE id = $1 RETURNING {FINDING_COLS}"
    )))
    .bind(id)
    .bind(review)
    .bind(by)
    .bind(note)
    .fetch_one(&mut *conn)
    .await
}

/// Marks the case's findings changed so `inference.findings.v1` is rewritten.
pub async fn touch_findings(conn: &mut PgConnection, case_id: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE inference.cases SET findings_revision = findings_revision + 1 WHERE case_id = $1",
    )
    .bind(case_id)
    .execute(conn)
    .await?;
    Ok(())
}

pub struct FindingFilter<'a> {
    pub case_id: Option<Uuid>,
    pub rule: Option<&'a str>,
    /// `None` = any status.
    pub status: Option<&'a str>,
    /// `none` = unreviewed.
    pub review: Option<&'a str>,
    pub severity: Option<&'a str>,
    pub limit: i64,
}

/// Newest first.
pub async fn findings(pool: &PgPool, f: &FindingFilter<'_>) -> sqlx::Result<Vec<Finding>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FINDING_COLS} FROM inference.findings
         WHERE ($1::uuid IS NULL OR case_id = $1) AND ($2::text IS NULL OR rule = $2)
           AND ($3::text IS NULL OR status = $3)
           AND ($4::text IS NULL OR ($4 = 'none' AND review IS NULL) OR review = $4)
           AND ($5::text IS NULL OR severity = $5)
         ORDER BY raised_at DESC, id DESC LIMIT $6"
    )))
    .bind(f.case_id)
    .bind(f.rule)
    .bind(f.status)
    .bind(f.review)
    .bind(f.severity)
    .bind(f.limit)
    .fetch_all(pool)
    .await
}

pub async fn unwritten(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT case_id FROM inference.cases WHERE written_revision < findings_revision
         ORDER BY updated_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Open findings for the artifact, with the case row, read consistently.
pub async fn artifact_state(
    pool: &PgPool,
    case_id: Uuid,
) -> sqlx::Result<Option<(CaseRow, Vec<Finding>)>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    let Some(c) = case(&mut tx, case_id).await? else {
        return Ok(None);
    };
    let f = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FINDING_COLS} FROM inference.findings WHERE case_id = $1 AND status = 'open'
         ORDER BY raised_at, id"
    )))
    .bind(case_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some((c, f)))
}

pub async fn mark_written(
    pool: &PgPool,
    case_id: Uuid,
    revision: i32,
    artifact_id: Uuid,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE inference.cases SET written_revision = GREATEST(written_revision, $2),
             findings_artifact_id = CASE WHEN $2 >= written_revision THEN $3 ELSE findings_artifact_id END
         WHERE case_id = $1",
    )
    .bind(case_id)
    .bind(revision)
    .bind(artifact_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn meta(pool: &PgPool, key: &str) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar("SELECT value FROM inference.meta WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
}

pub async fn set_meta(pool: &PgPool, key: &str, value: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO inference.meta (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

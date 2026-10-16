//! Case operations shared by the consumer and the HTTP API.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::json;
use shepherd_common::storage_client::{DocumentDetail, StorageClient, StorageError};
use shepherd_forms::{
    recognize::{FieldResult, FieldStatus, FormMatch},
    template::{FormTemplate, TemplateSet},
};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    benefits::{Benefit, FACTS},
    case::{self, Status},
    case_db::{self, Case},
    receipt::{self, Receipt},
};

pub const RECOGNITION_SCHEMA: &str = "intake.form.v1";

/// Final form values of a document: its latest receipt, else its latest recognition.
#[derive(Debug, Clone)]
pub struct FormValues {
    pub artifact_id: Uuid,
    /// Form type (edition-independent).
    pub form_type: Option<String>,
    /// Template (edition) the values were read or entered with.
    pub template_id: Option<String>,
    /// Field id → non-empty value.
    pub values: BTreeMap<String, String>,
    /// Flagged fields in a recognition (always 0 for receipts).
    pub requires_receipting: usize,
    pub receipt: Option<Receipt>,
}

impl FormValues {
    pub fn template<'a>(&self, templates: &'a TemplateSet) -> Option<&'a FormTemplate> {
        self.template_id.as_deref().and_then(|t| templates.get(t))
    }

    /// Values are final for `form_type`: receipted, or recognized with nothing flagged.
    pub fn is_final_for(&self, form_type: &str) -> bool {
        self.form_type.as_deref() == Some(form_type)
            && (self.receipt.is_some() || self.requires_receipting == 0)
    }
}

#[derive(Deserialize)]
struct RecognitionBody {
    form: Option<FormMatch>,
    fields: Vec<FieldResult>,
}

pub async fn form_values(
    storage: &StorageClient,
    doc: &DocumentDetail,
) -> anyhow::Result<Option<FormValues>> {
    if let Some(a) = doc.latest(receipt::SCHEMA) {
        let r: Receipt = serde_json::from_slice(&storage.content(a.id).await?)
            .map_err(|e| anyhow::anyhow!("receipt {}: {e}", a.id))?;
        return Ok(Some(FormValues {
            artifact_id: a.id,
            form_type: Some(r.form_type.clone()),
            template_id: Some(r.template_id().to_owned()),
            values: r.values.clone(),
            requires_receipting: 0,
            receipt: Some(r),
        }));
    }
    if let Some(a) = doc.latest(RECOGNITION_SCHEMA) {
        let r: RecognitionBody = serde_json::from_slice(&storage.content(a.id).await?)
            .map_err(|e| anyhow::anyhow!("recognition {}: {e}", a.id))?;
        return Ok(Some(FormValues {
            artifact_id: a.id,
            form_type: r.form.as_ref().map(|f| f.form_type().to_owned()),
            template_id: r.form.map(|f| f.template_id),
            values: r
                .fields
                .iter()
                .filter(|f| f.status == FieldStatus::Filled)
                .filter_map(|f| f.value.clone().map(|v| (f.id.clone(), v)))
                .collect(),
            requires_receipting: r
                .fields
                .iter()
                .filter(|f| f.status == FieldStatus::RequiresReceipting)
                .count(),
            receipt: None,
        }));
    }
    Ok(None)
}

/// Document detail, `None` when storage no longer has it.
pub async fn document(
    storage: &StorageClient,
    id: Uuid,
) -> Result<Option<DocumentDetail>, StorageError> {
    match storage.document(id).await {
        Ok(d) => Ok(Some(d)),
        Err(StorageError::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `receipt_number` label, normalized; invalid labels are ignored.
pub fn receipt_label(doc: &DocumentDetail) -> Option<String> {
    doc.labels
        .get("receipt_number")
        .and_then(|v| v.as_str())
        .and_then(case::normalize_receipt_number)
}

/// `received_date` label (`YYYY-MM-DD`), else the upload date.
pub fn received_date(doc: &DocumentDetail) -> time::Date {
    doc.labels
        .get("received_date")
        .and_then(|v| v.as_str())
        .and_then(|s| case::parse_date(s).ok())
        .or_else(|| doc.created_at.map(|t| t.date()))
        .unwrap_or_else(|| time::OffsetDateTime::now_utc().date())
}

pub struct Opening<'a> {
    /// Generated when `None`.
    pub receipt_number: Option<&'a str>,
    pub form_type: &'a str,
    pub document_id: Uuid,
    pub filename: Option<&'a str>,
    pub received_date: time::Date,
    pub facts: &'a BTreeMap<String, String>,
    pub actor: Option<&'a str>,
}

pub async fn open_case(
    conn: &mut PgConnection,
    receipt_prefix: &str,
    o: &Opening<'_>,
) -> sqlx::Result<Case> {
    let receipt = match o.receipt_number {
        Some(r) => r.to_owned(),
        None => {
            let seq = case_db::next_receipt_seq(conn).await?;
            case::receipt_number(receipt_prefix, o.received_date.year(), seq)
        }
    };
    let c = case_db::insert(
        conn,
        &case_db::NewCase {
            receipt_number: &receipt,
            form_type: o.form_type,
            primary_document_id: o.document_id,
            filename: o.filename,
            facts: o.facts,
            received_date: &case::format_date(o.received_date),
            actor: o.actor,
        },
    )
    .await?;
    case_db::log(
        conn,
        c.id,
        o.actor,
        "opened",
        json!({ "document_id": o.document_id, "form_type": o.form_type, "receipt_number": receipt }),
    )
    .await?;
    Ok(c)
}

/// Links a document to a case. While a notice is outstanding, a new document is recorded as the
/// response: the notice is closed and the case returns to review.
/// Returns the role and whether a response was recorded.
pub async fn attach(
    conn: &mut PgConnection,
    c: &Case,
    document_id: Uuid,
    filename: Option<&str>,
    role: Option<&str>,
    actor: Option<&str>,
) -> sqlx::Result<(String, bool)> {
    let awaiting = c.status == Status::AwaitingResponse.as_str();
    let role = role.unwrap_or(if awaiting { "response" } else { "supporting" });
    case_db::add_document(conn, c.id, document_id, role, filename, actor).await?;
    case_db::log(
        conn,
        c.id,
        actor,
        "document_added",
        json!({ "document_id": document_id, "role": role, "filename": filename }),
    )
    .await?;
    let responded = if role == "response" && awaiting {
        record_response(
            conn,
            c,
            actor,
            Some(&format!(
                "document {}",
                filename.unwrap_or(&document_id.to_string())
            )),
        )
        .await?
    } else {
        false
    };
    Ok((role.to_owned(), responded))
}

/// Closes the outstanding notice and returns the case to review.
pub async fn record_response(
    conn: &mut PgConnection,
    c: &Case,
    actor: Option<&str>,
    note: Option<&str>,
) -> sqlx::Result<bool> {
    let Some(n) = case_db::open_notice(conn, c.id).await? else {
        return Ok(false);
    };
    case_db::respond_notice(conn, n.id, note, actor).await?;
    case_db::set_status(
        conn,
        c.id,
        Status::working(c.assignee.as_deref()).as_str(),
        None,
    )
    .await?;
    case_db::log(
        conn,
        c.id,
        actor,
        "response_received",
        json!({ "notice_id": n.id, "kind": n.kind, "note": note }),
    )
    .await?;
    Ok(true)
}

/// Re-derives facts from form values, keeping officer overrides. Returns whether facts changed.
pub async fn refresh_facts(
    conn: &mut PgConnection,
    c: &Case,
    benefit: &Benefit,
    template: Option<&FormTemplate>,
    values: &BTreeMap<String, String>,
) -> sqlx::Result<bool> {
    if c.status == Status::Decided.as_str() {
        return Ok(false);
    }
    let derived = benefit.derive_facts(template, values);
    let mut facts = c.facts.0.clone();
    for (key, _) in FACTS {
        if c.overridden.iter().any(|o| o == key) {
            continue;
        }
        match derived.get(*key) {
            Some(v) => facts.insert((*key).to_owned(), v.clone()),
            None => facts.remove(*key),
        };
    }
    if facts == c.facts.0 {
        return Ok(false);
    }
    case_db::set_facts(conn, c.id, &facts, &c.overridden).await?;
    Ok(true)
}

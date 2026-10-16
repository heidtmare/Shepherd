//! shepherd-workbench client: the case data a letter is composed from.

use std::collections::BTreeMap;

use serde::Deserialize;
use uuid::Uuid;

use crate::letters::{self, CaseInfo};

#[derive(Debug, Deserialize)]
struct CaseRecord {
    receipt_number: String,
    form_type: String,
    received_date: String,
    #[serde(default)]
    facts: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct BenefitRef {
    name: String,
}

#[derive(Debug, Deserialize)]
struct Contact {
    email: Option<String>,
}

/// `GET /v1/cases/{id}`, the fields used here.
#[derive(Debug, Deserialize)]
struct CaseDetail {
    case: CaseRecord,
    benefit: Option<BenefitRef>,
    form_number: Option<String>,
    #[serde(default)]
    form_final: bool,
    e_notification: Option<Contact>,
}

#[derive(Clone)]
pub struct Workbench {
    http: reqwest::Client,
    base_url: String,
}

impl Workbench {
    pub fn new(base_url: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_owned(),
        }
    }

    /// `None` when the case does not exist.
    pub async fn case(&self, id: Uuid) -> anyhow::Result<Option<CaseInfo>> {
        let r = self
            .http
            .get(format!("{}/v1/cases/{id}", self.base_url))
            .header("x-user", crate::SERVICE)
            .send()
            .await?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let d: CaseDetail = r.error_for_status()?.json().await?;
        let received_date = letters::parse_date(&d.case.received_date)
            .ok_or_else(|| anyhow::anyhow!("case {id}: bad received_date"))?;
        Ok(Some(CaseInfo {
            case_id: id,
            form_number: d
                .form_number
                .unwrap_or_else(|| form_number(&d.case.form_type)),
            benefit: d
                .benefit
                .map(|b| b.name)
                .unwrap_or_else(|| d.case.form_type.clone()),
            receipt_number: d.case.receipt_number,
            form_type: d.case.form_type,
            received_date,
            facts: d.case.facts,
            form_final: d.form_final,
            email: d
                .e_notification
                .and_then(|c| c.email)
                .filter(|e| e.contains('@')),
        }))
    }

    pub async fn ready(&self) -> bool {
        self.http
            .get(format!("{}/readyz", self.base_url))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }
}

/// `uscis.i-765` → `I-765`.
fn form_number(form_type: &str) -> String {
    form_type
        .rsplit('.')
        .next()
        .unwrap_or(form_type)
        .to_uppercase()
}

#[cfg(test)]
mod tests {
    #[test]
    fn form_number_from_type() {
        assert_eq!(super::form_number("uscis.i-765"), "I-765");
    }
}

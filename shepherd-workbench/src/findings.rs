//! Case findings from shepherd-inference (`INFERENCE_URL`).
//!
//! - Proxy: the workbench lists a case's findings and records reviews through this service. Reviews
//!   are limited to the case's assignee and logged in the case history.
//! - Approval: open findings not dismissed block approval. Approval fails while inference is
//!   unavailable; other outcomes record findings when they can be read.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    AppState,
    api::{ApiError, ApiResult, user},
    case::Outcome,
    case_api::{ensure_assignee, ensure_undecided, load},
    case_db,
    correspondence::relay,
};

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/cases/{id}/findings", get(case_findings))
        .route("/v1/cases/{id}/findings/{fid}/review", post(review_finding))
}

/// Fields of an inference finding that the workbench reads and records at decision time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Finding {
    pub id: Uuid,
    pub rule: String,
    pub case_id: Uuid,
    pub severity: String,
    /// `open` or `resolved`.
    pub status: String,
    pub summary: String,
    #[serde(default)]
    pub related_cases: Vec<Uuid>,
    /// `confirmed` or `dismissed`.
    pub review: Option<String>,
    pub reviewed_by: Option<String>,
    pub review_note: Option<String>,
}

#[derive(Deserialize)]
struct FindingList {
    findings: Vec<Finding>,
}

/// Open findings that block approval: not dismissed.
pub fn blocking(findings: &[Finding]) -> usize {
    findings
        .iter()
        .filter(|f| f.status == "open" && f.review.as_deref() != Some("dismissed"))
        .count()
}

fn base(st: &AppState) -> Option<String> {
    st.cfg
        .inference_url
        .as_deref()
        .map(|u| u.trim_end_matches('/').to_owned())
}

fn upstream(e: reqwest::Error) -> ApiError {
    ApiError::Upstream(format!("inference unavailable: {e}"))
}

/// Open findings of the case; `None` when `INFERENCE_URL` is unset.
pub async fn open(st: &AppState, case_id: Uuid) -> ApiResult<Option<Vec<Finding>>> {
    let Some(base) = base(st) else {
        return Ok(None);
    };
    let list: FindingList = st
        .storage
        .http
        .get(format!("{base}/v1/findings"))
        .query(&[
            ("case_id", case_id.to_string()),
            ("status", "open".into()),
            ("limit", "500".into()),
        ])
        .timeout(TIMEOUT)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(upstream)?
        .json()
        .await
        .map_err(upstream)?;
    Ok(Some(list.findings))
}

/// Open findings for a decision. Approval requires them when inference is configured; other
/// outcomes proceed without them (`None`) when inference is unavailable.
pub async fn at_decision(
    st: &AppState,
    case_id: Uuid,
    outcome: Outcome,
) -> ApiResult<Option<Vec<Finding>>> {
    match open(st, case_id).await {
        Err(ApiError::Upstream(m)) if outcome != Outcome::Approved => {
            tracing::warn!(error = %m, case_id = %case_id, "findings not recorded with decision");
            Ok(None)
        }
        Err(ApiError::Upstream(m)) => Err(ApiError::Upstream(format!(
            "{m}; approval requires the finding check"
        ))),
        other => other,
    }
}

#[derive(Deserialize)]
struct ListParams {
    /// `open` (default), `resolved` or `all`.
    status: Option<String>,
}

/// Findings of the case, newest first; `enabled: false` when `INFERENCE_URL` is unset.
async fn case_findings(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Query(p): Query<ListParams>,
) -> ApiResult<Response> {
    load(&st, id).await?;
    let Some(base) = base(&st) else {
        return Ok(Json(json!({ "enabled": false, "findings": [] })).into_response());
    };
    let r = st
        .storage
        .http
        .get(format!("{base}/v1/findings"))
        .query(&[
            ("case_id", id.to_string()),
            ("status", p.status.unwrap_or_else(|| "open".into())),
            ("limit", "500".into()),
        ])
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(upstream)?;
    if !r.status().is_success() {
        return relay(r).await;
    }
    let mut v: Value = r.json().await.map_err(upstream)?;
    v["enabled"] = json!(true);
    Ok(Json(v).into_response())
}

#[derive(Deserialize, Serialize)]
struct Review {
    /// `confirmed`, `dismissed` (note required), or `null` to clear.
    disposition: Option<String>,
    note: Option<String>,
}

/// Record the assignee's review of a finding on the case.
async fn review_finding(
    State(st): State<AppState>,
    Path((id, fid)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    Json(b): Json<Review>,
) -> ApiResult<Response> {
    let actor = user(&headers)?;
    let base =
        base(&st).ok_or_else(|| ApiError::BadRequest("INFERENCE_URL not configured".into()))?;
    let c = load(&st, id).await?;
    ensure_undecided(&c)?;
    ensure_assignee(&c, &actor)?;
    let r = st
        .storage
        .http
        .get(format!("{base}/v1/findings/{fid}"))
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(upstream)?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(ApiError::NotFound);
    }
    let f: Finding = r
        .error_for_status()
        .map_err(upstream)?
        .json()
        .await
        .map_err(upstream)?;
    if f.case_id != id {
        return Err(ApiError::NotFound);
    }
    let r = st
        .storage
        .http
        .post(format!("{base}/v1/findings/{fid}/review"))
        .header("x-user", &actor)
        .timeout(TIMEOUT)
        .json(&b)
        .send()
        .await
        .map_err(upstream)?;
    if !r.status().is_success() {
        return relay(r).await;
    }
    let reviewed: Value = r.json().await.map_err(upstream)?;
    let note = b.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let mut tx = st.db.begin().await?;
    case_db::log(
        &mut tx,
        id,
        Some(&actor),
        "finding_reviewed",
        json!({ "finding_id": fid, "rule": f.rule, "summary": f.summary, "disposition": b.disposition, "note": note }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(reviewed).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(status: &str, review: Option<&str>) -> Finding {
        Finding {
            id: Uuid::now_v7(),
            rule: "a_number_name_mismatch".into(),
            case_id: Uuid::nil(),
            severity: "high".into(),
            status: status.into(),
            summary: "s".into(),
            related_cases: vec![],
            review: review.map(str::to_owned),
            reviewed_by: None,
            review_note: None,
        }
    }

    #[test]
    fn dismissed_and_resolved_do_not_block() {
        let f = [
            finding("open", None),
            finding("open", Some("confirmed")),
            finding("open", Some("dismissed")),
            finding("resolved", None),
        ];
        assert_eq!(blocking(&f), 2);
        assert_eq!(blocking(&[]), 0);
    }

    #[test]
    fn reads_inference_findings() {
        let v = json!({ "findings": [{
            "id": Uuid::nil(), "rule": "a_number_name_mismatch", "case_id": Uuid::nil(), "key": "012345678",
            "severity": "high", "status": "open", "summary": "A-Number A012345678 …",
            "evidence": { "a_number": "012345678" }, "related_cases": [Uuid::nil()],
            "review": null, "reviewed_by": null, "reviewed_at": null, "review_note": null,
            "raised_at": "2026-09-24T00:00:00Z", "updated_at": "2026-09-24T00:00:00Z", "resolved_at": null
        }] });
        let l: FindingList = serde_json::from_value(v).unwrap();
        assert_eq!(l.findings.len(), 1);
        assert_eq!(blocking(&l.findings), 1);
    }
}

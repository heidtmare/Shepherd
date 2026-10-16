//! Case discovery through shepherd-search.
//!
//! Cases are indexed as `workbench.case.v1` artifacts on their primary documents (see
//! `projection`). A query is sent to `POST /v1/search` and each hit is resolved to its case
//! through `case_documents`, so text matches in supporting evidence also find the case.
//! Receipt numbers and A-Numbers are additionally looked up exactly in the database.
//! Without `SEARCH_URL`, or when search fails, a database substring search is used.

use std::collections::BTreeMap;

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shepherd_forms::value::{self, ValueType};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    AppState,
    api::{ApiError, ApiResult},
    case::{self, CASE_SCHEMA, Outcome},
    case_api::parse_statuses,
    case_db::{self, Case},
};

const DEFAULT_LIMIT: u32 = 25;
const MAX_LIMIT: u32 = 100;
const FACETS: [&str; 3] = ["case_status", "benefit", "decision"];

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Match case records: receipt number, benefit, names, A-Numbers.
    #[default]
    Cases,
    /// Also match the text and form values of every document; hits resolve to their case, and
    /// documents in no case are returned separately.
    Documents,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryRequest {
    /// Web-search syntax (`"phrase"`, `-term`, `or`).
    pub q: Option<String>,
    #[serde(default)]
    pub status: Vec<String>,
    pub form_type: Option<String>,
    /// `approved`, `denied`, `withdrawn` or `closed`.
    pub decision: Option<String>,
    /// `YYYY-MM-DD`, inclusive.
    pub received_from: Option<String>,
    /// `YYYY-MM-DD`, inclusive.
    pub received_to: Option<String>,
    #[serde(default)]
    pub scope: Scope,
    /// `relevance` (default with `q`), `received_at` (default otherwise) or `updated_at`.
    pub sort: Option<String>,
    pub dir: Option<String>,
    pub cursor: Option<String>,
    /// 1..=100, default 25.
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DocumentMatch {
    pub document_id: Uuid,
    pub role: String,
    /// HTML-escaped snippet with matches in `<mark>`.
    pub highlight: Option<String>,
    pub matched_pages: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CaseHit {
    pub case: Case,
    pub matches: Vec<DocumentMatch>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DocumentHit {
    pub document_id: Uuid,
    pub filename: Option<String>,
    pub highlight: Option<String>,
    pub matched_pages: Vec<i64>,
    pub facets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DiscoveryResponse {
    /// `search` or `database`.
    pub source: &'static str,
    /// Cases whose receipt number or A-Number equals the query.
    pub exact: Vec<Case>,
    pub cases: Vec<CaseHit>,
    /// Matching documents that belong to no case (`documents` scope).
    pub documents: Vec<DocumentHit>,
    /// Facet name → [{value, count}] (`search` source, first page only).
    #[schema(value_type = Object)]
    pub facet_counts: Value,
    /// False when search counted only part of a large result set; counts are then lower bounds.
    pub facet_counts_exact: bool,
    pub next_cursor: Option<String>,
    /// Set when search was unavailable and the database fallback answered.
    pub warning: Option<String>,
}

struct Validated {
    q: Option<String>,
    statuses: Vec<String>,
    form_type: Option<String>,
    decision: Option<String>,
    from: Option<time::Date>,
    to: Option<time::Date>,
    limit: u32,
}

fn validate(st: &AppState, r: &DiscoveryRequest) -> ApiResult<Validated> {
    let q =
        r.q.as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_owned);
    if q.as_ref().is_some_and(|q| q.len() > 500) {
        return Err(ApiError::BadRequest("q exceeds 500 bytes".into()));
    }
    let joined = r.status.join(",");
    let statuses = parse_statuses(Some(&joined), &[])?;
    let form_type = r.form_type.clone().filter(|s| !s.is_empty());
    if let Some(ft) = &form_type
        && st.benefits.get(ft).is_none()
    {
        return Err(ApiError::BadRequest(format!(
            "{ft:?} is not a benefit form"
        )));
    }
    let decision = r.decision.clone().filter(|s| !s.is_empty());
    if let Some(d) = &decision
        && !Outcome::ALL.iter().any(|o| o.as_str() == d)
    {
        return Err(ApiError::BadRequest(format!("unknown decision {d:?}")));
    }
    let date = |s: &Option<String>, f: &str| -> ApiResult<Option<time::Date>> {
        s.as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| case::parse_date(s).map_err(|e| ApiError::BadRequest(format!("{f}: {e}"))))
            .transpose()
    };
    Ok(Validated {
        q,
        statuses,
        form_type,
        decision,
        from: date(&r.received_from, "received_from")?,
        to: date(&r.received_to, "received_to")?,
        limit: r.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
    })
}

/// Search request for shepherd-search.
fn search_body(v: &Validated, r: &DiscoveryRequest) -> ApiResult<Value> {
    let mut filters = Vec::new();
    let case_filters = !v.statuses.is_empty()
        || v.form_type.is_some()
        || v.decision.is_some()
        || v.from.is_some()
        || v.to.is_some();
    if r.scope == Scope::Cases || case_filters {
        filters.push(json!({"op": "in", "field": "schema", "values": [CASE_SCHEMA]}));
    }
    if !v.statuses.is_empty() {
        filters.push(json!({"op": "in", "field": "status", "values": v.statuses}));
    }
    if let Some(ft) = &v.form_type {
        filters.push(json!({"op": "facet", "value": format!("benefit/{ft}")}));
    }
    if let Some(d) = &v.decision {
        filters.push(json!({"op": "facet", "value": format!("decision/{d}")}));
    }
    if v.from.is_some() || v.to.is_some() {
        let mut range = json!({"op": "range", "field": "received_at"});
        if let Some(f) = v.from {
            range["gte"] = json!(format!("{}T00:00:00Z", case::format_date(f)));
        }
        if let Some(t) = v.to {
            let next = t.next_day().unwrap_or(t);
            range["lt"] = json!(format!("{}T00:00:00Z", case::format_date(next)));
        }
        filters.push(range);
    }
    let sort = match r.sort.as_deref() {
        None | Some("") => {
            if v.q.is_some() {
                "relevance"
            } else {
                "received_at"
            }
        }
        Some(s @ ("relevance" | "received_at" | "updated_at")) => s,
        Some(s) => return Err(ApiError::BadRequest(format!("unknown sort {s:?}"))),
    };
    let dir = match r.dir.as_deref() {
        None | Some("") | Some("desc") => "desc",
        Some("asc") => "asc",
        Some(d) => return Err(ApiError::BadRequest(format!("unknown dir {d:?}"))),
    };
    Ok(json!({
        "q": v.q,
        "filters": filters,
        // Counts cover the whole result set; later pages reuse the first page's.
        "facets": if r.cursor.is_none() { &FACETS[..] } else { &[] },
        "sort": {"field": sort, "dir": dir},
        "limit": v.limit,
        "cursor": r.cursor.as_deref().filter(|c| !c.starts_with("db:")),
        "include_pages": v.q.is_some() && r.scope == Scope::Documents,
    }))
}

enum SearchFailure {
    /// The request itself was rejected (bad cursor or filter).
    Rejected(String),
    Unavailable(String),
}

async fn query_search(st: &AppState, url: &str, body: &Value) -> Result<Value, SearchFailure> {
    let resp = st
        .storage
        .http
        .post(format!("{}/v1/search", url.trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(5))
        .json(body)
        .send()
        .await
        .map_err(|e| SearchFailure::Unavailable(e.to_string()))?;
    let status = resp.status();
    if status.is_client_error() {
        let v: Value = resp.json().await.unwrap_or_default();
        return Err(SearchFailure::Rejected(
            v["message"]
                .as_str()
                .unwrap_or("search rejected the query")
                .to_owned(),
        ));
    }
    if !status.is_success() {
        return Err(SearchFailure::Unavailable(format!(
            "search returned {status}"
        )));
    }
    resp.json()
        .await
        .map_err(|e| SearchFailure::Unavailable(e.to_string()))
}

async fn exact(st: &AppState, q: Option<&str>) -> ApiResult<Vec<Case>> {
    let Some(q) = q else { return Ok(vec![]) };
    if let Some(rn) = case::normalize_receipt_number(q) {
        let mut conn = st.db.acquire().await?;
        return Ok(case_db::by_receipt(&mut conn, &rn)
            .await?
            .into_iter()
            .collect());
    }
    let looks_like_a_number = q.len() <= 14
        && q.chars().any(|c| c.is_ascii_digit())
        && q.trim_start_matches(['A', 'a'])
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | ' '));
    if looks_like_a_number && let Ok(a) = value::normalize(ValueType::ANumber, q, None, &[]) {
        return Ok(case_db::by_a_number(&st.db, &a).await?);
    }
    Ok(vec![])
}

async fn from_search(
    st: &AppState,
    hits: &Value,
    scope: Scope,
) -> ApiResult<(Vec<CaseHit>, Vec<DocumentHit>)> {
    let hits = hits.as_array().cloned().unwrap_or_default();
    let ids: Vec<Uuid> = hits
        .iter()
        .filter_map(|h| h["doc_id"].as_str()?.parse().ok())
        .collect();
    let matches = case_db::for_documents(&st.db, &ids).await?;
    let by_doc: BTreeMap<Uuid, &case_db::CaseMatch> =
        matches.iter().map(|m| (m.matched_document_id, m)).collect();
    let mut cases: Vec<CaseHit> = Vec::new();
    let mut documents = Vec::new();
    for h in &hits {
        let Some(id) = h["doc_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else {
            continue;
        };
        let highlight = h["highlight"].as_str().map(str::to_owned);
        let pages: Vec<i64> = h["matched_pages"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        match by_doc.get(&id) {
            Some(m) => {
                let dm = DocumentMatch {
                    document_id: id,
                    role: m.matched_role.clone(),
                    highlight,
                    matched_pages: pages,
                };
                match cases.iter_mut().find(|c| c.case.id == m.case.id) {
                    Some(c) => c.matches.push(dm),
                    None => cases.push(CaseHit {
                        case: m.case.clone(),
                        matches: vec![dm],
                    }),
                }
            }
            None if scope == Scope::Documents => documents.push(DocumentHit {
                document_id: id,
                filename: h["summary"]["filename"].as_str().map(str::to_owned),
                highlight,
                matched_pages: pages,
                facets: h["summary"]["facets"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|f| f.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            // Projection of a case not visible in this database (or stale); skip.
            None => {}
        }
    }
    Ok((cases, documents))
}

async fn from_database(
    st: &AppState,
    v: &Validated,
    cursor: Option<&str>,
) -> ApiResult<(Vec<CaseHit>, Option<String>)> {
    let offset: i64 = cursor
        .and_then(|c| c.strip_prefix("db:"))
        .and_then(|o| o.parse().ok())
        .unwrap_or(0);
    let limit = v.limit as i64;
    let from = v.from.map(case::format_date);
    let to = v.to.map(case::format_date);
    let items = case_db::search(
        &st.db,
        &case_db::SearchFilter {
            q: v.q.as_deref(),
            statuses: &v.statuses,
            form_type: v.form_type.as_deref(),
            decision: v.decision.as_deref(),
            received_from: from.as_deref(),
            received_to: to.as_deref(),
            limit,
            offset,
        },
    )
    .await?;
    let next = (items.len() as i64 == limit).then(|| format!("db:{}", offset + limit));
    Ok((
        items
            .into_iter()
            .map(|c| CaseHit {
                case: c,
                matches: vec![],
            })
            .collect(),
        next,
    ))
}

/// Find cases by free text, receipt number, A-Number, status, benefit, decision and received date.
#[utoipa::path(post, path = "/v1/cases/search", request_body = DiscoveryRequest,
    responses((status = 200, body = DiscoveryResponse), (status = 400)))]
pub async fn search(
    State(st): State<AppState>,
    Json(r): Json<DiscoveryRequest>,
) -> ApiResult<Json<DiscoveryResponse>> {
    let v = validate(&st, &r)?;
    let exact = if r.cursor.is_none() {
        exact(&st, v.q.as_deref()).await?
    } else {
        vec![]
    };
    let db_cursor = r.cursor.as_deref().is_some_and(|c| c.starts_with("db:"));
    let mut warning = None;
    if let Some(url) = st.cfg.search_url.as_deref()
        && !db_cursor
    {
        let body = search_body(&v, &r)?;
        match query_search(&st, url, &body).await {
            Ok(resp) => {
                let (cases, documents) = from_search(&st, &resp["hits"], r.scope).await?;
                metrics::counter!("case_discovery_total", "source" => "search").increment(1);
                return Ok(Json(DiscoveryResponse {
                    source: "search",
                    exact,
                    cases,
                    documents,
                    facet_counts: resp["facet_counts"].clone(),
                    facet_counts_exact: resp["facet_counts_exact"].as_bool().unwrap_or(true),
                    next_cursor: resp["next_cursor"].as_str().map(str::to_owned),
                    warning: None,
                }));
            }
            Err(SearchFailure::Rejected(m)) => return Err(ApiError::BadRequest(m)),
            Err(SearchFailure::Unavailable(e)) => {
                tracing::warn!(error = %e, "search unavailable; using database");
                warning = Some("search unavailable; showing database matches".to_owned());
            }
        }
    }
    let (cases, next_cursor) = from_database(&st, &v, r.cursor.as_deref()).await?;
    metrics::counter!("case_discovery_total", "source" => "database").increment(1);
    Ok(Json(DiscoveryResponse {
        source: "database",
        exact,
        cases,
        documents: vec![],
        facet_counts: json!({}),
        facet_counts_exact: true,
        next_cursor,
        warning,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(q: Option<&str>) -> Validated {
        Validated {
            q: q.map(str::to_owned),
            statuses: vec![],
            form_type: None,
            decision: None,
            from: None,
            to: None,
            limit: 25,
        }
    }

    #[test]
    fn search_body_scopes_and_filters() {
        let r = DiscoveryRequest::default();
        let b = search_body(&v(None), &r).unwrap();
        assert_eq!(b["filters"][0]["values"][0], CASE_SCHEMA);
        assert_eq!(b["sort"]["field"], "received_at");
        assert_eq!(b["include_pages"], false);

        let r = DiscoveryRequest {
            scope: Scope::Documents,
            ..Default::default()
        };
        let b = search_body(&v(Some("passport")), &r).unwrap();
        assert!(b["filters"].as_array().unwrap().is_empty());
        assert_eq!(
            (b["sort"]["field"].as_str(), b["include_pages"].as_bool()),
            (Some("relevance"), Some(true))
        );

        let mut val = v(Some("nguyen"));
        val.statuses = vec!["open".into()];
        val.form_type = Some("uscis.i-765".into());
        val.decision = Some("approved".into());
        val.from = Some(time::macros::date!(2026 - 01 - 01));
        val.to = Some(time::macros::date!(2026 - 01 - 31));
        let b = search_body(&val, &r).unwrap();
        let f = b["filters"].as_array().unwrap();
        assert_eq!(f[0]["field"], "schema");
        assert_eq!(f[2]["value"], "benefit/uscis.i-765");
        assert_eq!(f[3]["value"], "decision/approved");
        assert_eq!(f[4]["gte"], "2026-01-01T00:00:00Z");
        assert_eq!(f[4]["lt"], "2026-02-01T00:00:00Z");

        let bad = DiscoveryRequest {
            sort: Some("amount".into()),
            ..Default::default()
        };
        assert!(search_body(&v(None), &bad).is_err());
        let dbc = DiscoveryRequest {
            cursor: Some("db:25".into()),
            ..Default::default()
        };
        assert!(search_body(&v(None), &dbc).unwrap()["cursor"].is_null());

        // Facets are counted on the first page only.
        assert_eq!(b["facets"].as_array().unwrap().len(), FACETS.len());
        let next = DiscoveryRequest {
            cursor: Some("abc".into()),
            ..Default::default()
        };
        assert!(
            search_body(&v(None), &next).unwrap()["facets"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}

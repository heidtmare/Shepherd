//! HTTP request/response types.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    /// Web-search syntax: `"exact phrase"`, `-exclude`, `a or b`. Also fuzzy-matches filenames.
    pub q: Option<String>,
    #[serde(default)]
    pub filters: Vec<Filter>,
    /// Facet names to count over the full match set.
    #[serde(default)]
    pub facets: Vec<String>,
    pub sort: Option<Sort>,
    /// 1..=100, default 20.
    pub limit: Option<u32>,
    pub cursor: Option<String>,
    /// Also match per-page text and return matching page numbers.
    #[serde(default)]
    pub include_pages: bool,
}

/// Filterable fields: `filename`, `created_at`, `updated_at`, `schema`, and the columns declared in
/// `mapping.toml`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Filter {
    Eq {
        field: String,
        value: Value,
    },
    In {
        field: String,
        values: Vec<Value>,
    },
    Range {
        field: String,
        gte: Option<Value>,
        gt: Option<Value>,
        lte: Option<Value>,
        lt: Option<Value>,
    },
    /// JSONB containment against the card's `fields` (`{"<schema>": {...}}`).
    Contains {
        #[schema(value_type = Object)]
        value: Value,
    },
    /// Exact facet match, `name/value`.
    Facet {
        value: String,
    },
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Sort {
    /// `relevance`, `updated_at`, `created_at`, `filename`, or a declared column with `sort = true`.
    pub field: String,
    #[serde(default)]
    pub dir: Dir,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    Asc,
    #[default]
    Desc,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SearchResponse {
    pub hits: Vec<Hit>,
    pub facet_counts: BTreeMap<String, Vec<FacetCount>>,
    /// False when the match set exceeded the facet window; counts are then lower bounds.
    pub facet_counts_exact: bool,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Hit {
    pub doc_id: Uuid,
    pub score: f32,
    /// HTML-escaped snippet with matches wrapped in `<mark>`.
    pub highlight: Option<String>,
    pub matched_pages: Vec<i32>,
    pub summary: CardSummary,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CardSummary {
    pub filename: Option<String>,
    /// Declared columns (`mapping.toml` `[columns]`) by name: text, numeric as a string, or an
    /// RFC 3339 timestamp; null when unset.
    #[serde(flatten)]
    #[schema(value_type = Object)]
    pub columns: BTreeMap<String, Value>,
    pub facets: Vec<String>,
    pub schemas: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct FacetCount {
    pub value: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CardView {
    pub doc_id: Uuid,
    #[serde(flatten)]
    pub summary: CardSummary,
    #[schema(value_type = Object)]
    pub fields: Value,
}

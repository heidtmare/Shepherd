//! Engine-independent validation of search requests.
//!
//! Field names come from a fixed whitelist and values are normalized here, so backends only ever
//! interpolate identifiers from this module and bind every user-supplied value.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    mapping::{ColumnType, Columns, valid_facet_name},
    model::{Dir, Filter, SearchRequest},
};

pub const DEFAULT_LIMIT: u32 = 20;
pub const MAX_LIMIT: u32 = 100;
const MAX_Q_LEN: usize = 1000;
const MAX_FILTERS: usize = 32;
const MAX_FACETS: usize = 20;
const MAX_IN_VALUES: usize = 1000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QueryError {
    #[error("unknown field {0:?}")]
    UnknownField(String),
    #[error("invalid value for {field}: {reason}")]
    InvalidValue { field: String, reason: String },
    #[error("{0}")]
    Invalid(String),
    #[error("cursor does not match this query")]
    BadCursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Numeric,
    Timestamp,
    /// Multi-valued text; `eq`/`in` test membership.
    TextList,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    Filename,
    CreatedAt,
    UpdatedAt,
    Schema,
    /// Declared in `mapping.toml` `[columns]`; the name is a validated identifier.
    Column {
        name: String,
        ty: ColumnType,
    },
}

impl Field {
    pub fn parse(s: &str, columns: &Columns) -> Result<Self, QueryError> {
        Ok(match s {
            "filename" => Self::Filename,
            "created_at" => Self::CreatedAt,
            "updated_at" => Self::UpdatedAt,
            "schema" => Self::Schema,
            _ => match columns.get(s) {
                Some(def) => Self::Column {
                    name: s.to_owned(),
                    ty: def.ty,
                },
                None => return Err(QueryError::UnknownField(s.into())),
            },
        })
    }

    pub fn kind(&self) -> FieldKind {
        match self {
            Self::Filename => FieldKind::Text,
            Self::CreatedAt | Self::UpdatedAt => FieldKind::Timestamp,
            Self::Schema => FieldKind::TextList,
            Self::Column { ty, .. } => match ty {
                ColumnType::Text => FieldKind::Text,
                ColumnType::Numeric => FieldKind::Numeric,
                ColumnType::Timestamp => FieldKind::Timestamp,
            },
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Filename => "filename",
            Self::CreatedAt => "created_at",
            Self::UpdatedAt => "updated_at",
            Self::Schema => "schema",
            Self::Column { name, .. } => name,
        }
    }

    /// Returns the value in canonical text form for binding with a type cast.
    fn normalize(&self, v: &Value) -> Result<String, QueryError> {
        let bad = |reason: &str| QueryError::InvalidValue {
            field: self.name().into(),
            reason: reason.into(),
        };
        match self.kind() {
            FieldKind::Text | FieldKind::TextList => match v {
                Value::String(s) => Ok(s.clone()),
                _ => Err(bad("expected string")),
            },
            FieldKind::Numeric => {
                let s = match v {
                    Value::Number(n) => n.to_string(),
                    Value::String(s) => s.trim().to_owned(),
                    _ => return Err(bad("expected number")),
                };
                match s.parse::<f64>() {
                    Ok(f) if f.is_finite() => Ok(s),
                    _ => Err(bad("expected finite number")),
                }
            }
            FieldKind::Timestamp => {
                let s = v.as_str().ok_or_else(|| bad("expected RFC 3339 string"))?;
                let t = OffsetDateTime::parse(s, &Rfc3339)
                    .map_err(|_| bad("expected RFC 3339 string"))?;
                Ok(t.format(&Rfc3339)
                    .expect("RFC 3339 formatting is infallible for parsed values"))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Gte,
    Gt,
    Lte,
    Lt,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    Cmp {
        field: Field,
        op: CmpOp,
        value: String,
    },
    AnyOf {
        field: Field,
        values: Vec<String>,
    },
    Contains(Value),
    Facet(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SortKey {
    Relevance,
    UpdatedAt,
    CreatedAt,
    Filename,
    /// A declared column with `sort = true` (numeric or timestamp).
    Column {
        name: String,
        ty: ColumnType,
    },
}

impl SortKey {
    fn parse(s: &str, columns: &Columns) -> Result<Self, QueryError> {
        Ok(match s {
            "relevance" => Self::Relevance,
            "updated_at" => Self::UpdatedAt,
            "created_at" => Self::CreatedAt,
            "filename" => Self::Filename,
            _ => match columns.get(s) {
                Some(def) if def.sort => Self::Column {
                    name: s.to_owned(),
                    ty: def.ty,
                },
                _ => return Err(QueryError::Invalid(format!("unknown sort field {s:?}"))),
            },
        })
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Relevance => "relevance",
            Self::UpdatedAt => "updated_at",
            Self::CreatedAt => "created_at",
            Self::Filename => "filename",
            Self::Column { name, .. } => name,
        }
    }
}

/// Keyset position: the last hit's sort value (backend text form) and id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    #[serde(rename = "k")]
    pub sort: String,
    #[serde(rename = "d")]
    pub dir: Dir,
    #[serde(rename = "s")]
    pub value: String,
    pub id: Uuid,
}

impl Cursor {
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).expect("cursor serializes"))
    }

    pub fn decode(s: &str) -> Result<Self, QueryError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|_| QueryError::BadCursor)?;
        serde_json::from_slice(&bytes).map_err(|_| QueryError::BadCursor)
    }
}

/// A validated search request.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub q: Option<String>,
    pub clauses: Vec<Clause>,
    pub facets: Vec<String>,
    pub sort: SortKey,
    pub dir: Dir,
    pub limit: u32,
    pub after: Option<Cursor>,
    pub include_pages: bool,
}

impl Plan {
    pub fn new(req: &SearchRequest, columns: &Columns) -> Result<Self, QueryError> {
        let q = req
            .q
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if q.as_ref().is_some_and(|q| q.len() > MAX_Q_LEN) {
            return Err(QueryError::Invalid(format!("q exceeds {MAX_Q_LEN} bytes")));
        }
        if req.filters.len() > MAX_FILTERS {
            return Err(QueryError::Invalid(format!(
                "at most {MAX_FILTERS} filters"
            )));
        }
        if req.facets.len() > MAX_FACETS {
            return Err(QueryError::Invalid(format!("at most {MAX_FACETS} facets")));
        }

        let mut clauses = Vec::with_capacity(req.filters.len());
        for f in &req.filters {
            match f {
                Filter::Eq { field, value } => {
                    let field = Field::parse(field, columns)?;
                    let value = field.normalize(value)?;
                    clauses.push(Clause::Cmp {
                        field,
                        op: CmpOp::Eq,
                        value,
                    });
                }
                Filter::In { field, values } => {
                    let field = Field::parse(field, columns)?;
                    if values.is_empty() || values.len() > MAX_IN_VALUES {
                        return Err(QueryError::Invalid(format!(
                            "`in` takes 1..={MAX_IN_VALUES} values"
                        )));
                    }
                    let values = values
                        .iter()
                        .map(|v| field.normalize(v))
                        .collect::<Result<_, _>>()?;
                    clauses.push(Clause::AnyOf { field, values });
                }
                Filter::Range {
                    field,
                    gte,
                    gt,
                    lte,
                    lt,
                } => {
                    let field = Field::parse(field, columns)?;
                    if field.kind() == FieldKind::TextList {
                        return Err(QueryError::Invalid(format!(
                            "range not supported on {:?}",
                            field.name()
                        )));
                    }
                    let bounds = [
                        (CmpOp::Gte, gte),
                        (CmpOp::Gt, gt),
                        (CmpOp::Lte, lte),
                        (CmpOp::Lt, lt),
                    ];
                    let before = clauses.len();
                    for (op, v) in bounds {
                        if let Some(v) = v {
                            let value = field.normalize(v)?;
                            clauses.push(Clause::Cmp {
                                field: field.clone(),
                                op,
                                value,
                            });
                        }
                    }
                    if clauses.len() == before {
                        return Err(QueryError::Invalid(
                            "range requires at least one bound".into(),
                        ));
                    }
                }
                Filter::Contains { value } => {
                    if !value.is_object() {
                        return Err(QueryError::Invalid(
                            "contains value must be an object".into(),
                        ));
                    }
                    clauses.push(Clause::Contains(value.clone()));
                }
                Filter::Facet { value } => {
                    let (name, v) = value.split_once('/').ok_or_else(|| {
                        QueryError::Invalid("facet filter must be name/value".into())
                    })?;
                    if !valid_facet_name(name) || v.is_empty() {
                        return Err(QueryError::Invalid(format!(
                            "invalid facet filter {value:?}"
                        )));
                    }
                    clauses.push(Clause::Facet(value.clone()));
                }
            }
        }

        let mut facets = Vec::with_capacity(req.facets.len());
        for name in &req.facets {
            if !valid_facet_name(name) {
                return Err(QueryError::Invalid(format!("invalid facet name {name:?}")));
            }
            if !facets.contains(name) {
                facets.push(name.clone());
            }
        }

        let requested = req
            .sort
            .as_ref()
            .map(|s| SortKey::parse(&s.field, columns))
            .transpose()?;
        let dir = req.sort.as_ref().map(|s| s.dir).unwrap_or_default();
        let sort = match (requested, &q) {
            (Some(SortKey::Relevance) | None, Some(_)) => SortKey::Relevance,
            (Some(SortKey::Relevance) | None, None) => SortKey::UpdatedAt,
            (Some(k), _) => k,
        };

        let after = req.cursor.as_deref().map(Cursor::decode).transpose()?;
        if let Some(c) = &after
            && (c.sort != sort.name() || c.dir != dir)
        {
            return Err(QueryError::BadCursor);
        }

        Ok(Self {
            q,
            clauses,
            facets,
            sort,
            dir,
            limit: req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
            after,
            include_pages: req.include_pages,
        })
    }
}

impl Plan {
    /// Identifies the match set and requested facets; sort, cursor and limit do not affect counts.
    pub fn facet_key(&self) -> String {
        format!(
            "{:?}",
            (&self.q, &self.clauses, &self.facets, self.include_pages)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(v: Value) -> SearchRequest {
        serde_json::from_value(v).unwrap()
    }

    fn columns() -> Columns {
        toml::from_str::<crate::mapping::Mapping>(include_str!("../config/mapping.toml"))
            .unwrap()
            .columns
    }

    fn plan(v: Value) -> Result<Plan, QueryError> {
        Plan::new(&req(v), &columns())
    }

    #[test]
    fn defaults() {
        let p = plan(json!({})).unwrap();
        assert_eq!(
            (p.sort, p.dir, p.limit),
            (SortKey::UpdatedAt, Dir::Desc, DEFAULT_LIMIT)
        );
        let p = plan(json!({"q": "  water  ", "limit": 1000})).unwrap();
        assert_eq!(
            (p.q.as_deref(), p.sort, p.limit),
            (Some("water"), SortKey::Relevance, MAX_LIMIT)
        );
    }

    #[test]
    fn whitelist_rejects_unknown_fields() {
        let e =
            plan(json!({"filters": [{"op": "eq", "field": "doc_id; DROP TABLE x", "value": "a"}]}));
        assert!(matches!(e, Err(QueryError::UnknownField(_))));
        assert!(plan(json!({"sort": {"field": "content"}})).is_err());
        // Declared but not sortable.
        assert!(plan(json!({"sort": {"field": "status"}})).is_err());
        assert!(plan(json!({"sort": {"field": "received_at"}})).is_ok());
        assert!(plan(json!({"facets": ["Type"]})).is_err());
        assert!(plan(json!({"filters": [{"op": "facet", "value": "nope"}]})).is_err());
    }

    #[test]
    fn value_normalization() {
        let p = plan(json!({"filters": [
            {"op": "range", "field": "amount", "gte": 10, "lt": "20.5"},
            {"op": "eq", "field": "received_at", "value": "2026-01-02T03:04:05+02:00"},
        ]}))
        .unwrap();
        assert_eq!(p.clauses.len(), 3);
        assert_eq!(
            p.clauses[1],
            Clause::Cmp {
                field: Field::Column {
                    name: "amount".into(),
                    ty: ColumnType::Numeric
                },
                op: CmpOp::Lt,
                value: "20.5".into()
            }
        );
        assert!(
            plan(json!({"filters": [{"op": "eq", "field": "amount", "value": "1e999"}]})).is_err()
        );
        assert!(
            plan(json!({"filters": [{"op": "eq", "field": "created_at", "value": "today"}]}))
                .is_err()
        );
        assert!(
            plan(json!({"filters": [{"op": "range", "field": "schema", "gte": "a"}]})).is_err()
        );
        assert!(plan(json!({"filters": [{"op": "range", "field": "amount"}]})).is_err());
    }

    #[test]
    fn cursor_round_trip_and_binding() {
        let c = Cursor {
            sort: "amount".into(),
            dir: Dir::Asc,
            value: "12.5".into(),
            id: Uuid::now_v7(),
        };
        assert_eq!(Cursor::decode(&c.encode()).unwrap(), c);
        let ok = req(json!({"sort": {"field": "amount", "dir": "asc"}, "cursor": c.encode()}));
        assert_eq!(Plan::new(&ok, &columns()).unwrap().after, Some(c.clone()));
        let mismatch =
            req(json!({"sort": {"field": "amount", "dir": "desc"}, "cursor": c.encode()}));
        assert_eq!(Plan::new(&mismatch, &columns()), Err(QueryError::BadCursor));
        assert_eq!(plan(json!({"cursor": "!!"})), Err(QueryError::BadCursor));
    }
}

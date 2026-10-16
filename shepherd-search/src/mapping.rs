//! Derived JSON → card projection, driven by `mapping.toml`.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use jsonpath_rust::JsonPath;
use serde::Deserialize;
use serde_json::{Map, Value};
use time::{
    Date, OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339,
    macros::format_description,
};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    /// Typed card columns for filters and sorts, by name. Created on startup (`columns::sync`).
    #[serde(default)]
    pub columns: Columns,
    #[serde(default, rename = "schema")]
    pub schemas: Vec<SchemaMapping>,
    /// Schemas whose bodies are never fetched. They still get the `schema/<name>` facet.
    #[serde(default)]
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaMapping {
    pub name: String,
    #[serde(default)]
    pub content: Vec<String>,
    pub pages: Option<PageMapping>,
    #[serde(default)]
    /// Declared column name → JSONPath.
    pub columns: BTreeMap<String, String>,
    #[serde(default)]
    pub facets: BTreeMap<String, String>,
    /// Copy the body into `cards.fields` (for `contains` filters and `GET /v1/cards`). Disable for
    /// bulky schemas whose searchable text already lands in `content`/`pages`.
    #[serde(default = "default_true")]
    pub fields: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageMapping {
    pub path: String,
    pub number: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ColumnType {
    Text,
    Numeric,
    Timestamp,
}

impl ColumnType {
    pub fn sql(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Numeric => "numeric",
            Self::Timestamp => "timestamptz",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ColumnDef {
    #[serde(rename = "type")]
    pub ty: ColumnType,
    /// Sortable (numeric and timestamp only); adds keyset indexes for both directions.
    #[serde(default)]
    pub sort: bool,
}

pub type Columns = BTreeMap<String, ColumnDef>;

/// Names used by built-in card columns, filters or sorts.
const RESERVED_COLUMNS: &[&str] = &[
    "doc_id",
    "filename",
    "content",
    "fields",
    "facets",
    "schemas",
    "schema",
    "source_seq",
    "created_at",
    "updated_at",
    "indexed_at",
    "tsv",
    "relevance",
];

pub fn valid_column_name(s: &str) -> bool {
    s.len() <= 40
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
        && !RESERVED_COLUMNS.contains(&s)
}

/// Reserved facet populated from the artifact schemas present on a document.
pub const SCHEMA_FACET: &str = "schema";

pub fn valid_facet_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

impl Mapping {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let m: Mapping = toml::from_str(&std::fs::read_to_string(path)?)?;
        m.validate()?;
        Ok(m)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let check = |p: &str| {
            Value::Null
                .query(p)
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!("invalid JSONPath {p:?}: {e}"))
        };
        for (name, def) in &self.columns {
            anyhow::ensure!(valid_column_name(name), "invalid column name {name:?}");
            anyhow::ensure!(
                !def.sort || def.ty != ColumnType::Text,
                "column {name:?}: text columns are not sortable"
            );
        }
        for s in &self.schemas {
            s.content.iter().try_for_each(|p| check(p))?;
            for (col, p) in &s.columns {
                anyhow::ensure!(
                    self.columns.contains_key(col),
                    "schema {} maps undeclared column {col:?}",
                    s.name
                );
                check(p)?;
            }
            for (name, p) in &s.facets {
                anyhow::ensure!(
                    valid_facet_name(name) && name != SCHEMA_FACET,
                    "invalid facet name {name:?} in schema {}",
                    s.name
                );
                check(p)?;
            }
            if let Some(pg) = &s.pages {
                check(&pg.path)?;
                check(&pg.text)?;
                pg.number.as_deref().map(check).transpose()?;
            }
            anyhow::ensure!(
                !self.ignored(&s.name),
                "schema {} is both mapped and ignored",
                s.name
            );
        }
        Ok(())
    }

    pub fn ignored(&self, schema: &str) -> bool {
        self.ignore.iter().any(|s| s == schema)
    }
}

/// Everything the mapper needs about one document, loaded from the storage schema.
#[derive(Debug, Clone)]
pub struct DocumentSource {
    pub doc_id: Uuid,
    pub filename: Option<String>,
    pub created_at: OffsetDateTime,
    /// Latest version per derived schema.
    pub artifacts: Vec<DerivedArtifact>,
}

#[derive(Debug, Clone)]
pub struct DerivedArtifact {
    pub schema: String,
    pub created_at: OffsetDateTime,
    pub body: Arc<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub doc_id: Uuid,
    pub filename: Option<String>,
    pub content: String,
    pub fields: Value,
    /// Declared column → canonical text (numeric as written, timestamps RFC 3339).
    pub columns: BTreeMap<String, String>,
    /// Decimal string, validated as a finite number.
    pub facets: Vec<String>,
    pub schemas: Vec<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub page_no: i32,
    pub text: String,
    /// The page lies entirely within the part of `content` that `cards.tsv` indexes, so a card
    /// match already covers it.
    pub in_card: bool,
}

/// Characters of `cards.content` indexed by `cards.tsv` (`migrations/0001_search.sql`).
pub const CARD_TSV_CHARS: usize = 300_000;

fn select<'a>(v: &'a Value, path: &str) -> Vec<&'a Value> {
    v.query(path).unwrap_or_default()
}

fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Flattens matches to strings; arrays contribute their scalar elements.
fn texts(v: &Value, path: &str) -> Vec<String> {
    let mut out = Vec::new();
    for m in select(v, path) {
        match m {
            Value::Array(items) => out.extend(items.iter().filter_map(scalar_text)),
            other => out.extend(scalar_text(other)),
        }
    }
    out
}

fn parse_amount(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(|_| s.trim().to_owned()),
        _ => None,
    }
}

fn parse_timestamp(v: &Value) -> Option<OffsetDateTime> {
    let s = v.as_str()?.trim();
    OffsetDateTime::parse(s, &Rfc3339).ok().or_else(|| {
        Date::parse(s, format_description!("[year]-[month]-[day]"))
            .ok()
            .map(|d| d.midnight().assume_utc())
    })
}

pub fn build(mapping: &Mapping, src: &DocumentSource) -> (Card, Vec<Page>) {
    let mut content: Vec<String> = Vec::new();
    let mut pages: Vec<Page> = Vec::new();
    let mut facets: BTreeSet<String> = BTreeSet::new();
    let mut fields = Map::new();
    let mut columns: BTreeMap<String, String> = BTreeMap::new();
    let mut updated_at = src.created_at;

    let by_schema: BTreeMap<&str, &DerivedArtifact> = src
        .artifacts
        .iter()
        .map(|a| (a.schema.as_str(), a))
        .collect();
    let excluded: BTreeSet<&str> = mapping
        .schemas
        .iter()
        .filter(|s| !s.fields)
        .map(|s| s.name.as_str())
        .chain(mapping.ignore.iter().map(String::as_str))
        .collect();
    for a in &src.artifacts {
        if !excluded.contains(a.schema.as_str()) {
            fields.insert(a.schema.clone(), (*a.body).clone());
        }
        facets.insert(format!("{SCHEMA_FACET}/{}", a.schema));
        updated_at = updated_at.max(a.created_at);
    }

    for sm in &mapping.schemas {
        let Some(a) = by_schema.get(sm.name.as_str()) else {
            continue;
        };
        let body: &Value = &a.body;
        for p in &sm.content {
            content.extend(texts(body, p));
        }
        if pages.is_empty()
            && let Some(pm) = &sm.pages
        {
            for (i, page) in select(body, &pm.path).into_iter().enumerate() {
                let text = texts(page, &pm.text).join("\n");
                if text.is_empty() {
                    continue;
                }
                let page_no = pm
                    .number
                    .as_deref()
                    .and_then(|p| select(page, p).first().and_then(|v| v.as_i64()))
                    .and_then(|n| i32::try_from(n).ok())
                    .unwrap_or(i as i32 + 1);
                pages.push(Page {
                    page_no,
                    text,
                    in_card: false,
                });
            }
        }
        for (col, p) in &sm.columns {
            let (Some(def), false) = (mapping.columns.get(col), columns.contains_key(col)) else {
                continue;
            };
            let Some(first) = select(body, p).into_iter().next() else {
                continue;
            };
            let v = match def.ty {
                ColumnType::Text => scalar_text(first),
                ColumnType::Numeric => parse_amount(first),
                ColumnType::Timestamp => parse_timestamp(first).map(|t| {
                    t.to_offset(UtcOffset::UTC)
                        .format(&Rfc3339)
                        .expect("RFC 3339 formats any UTC time")
                }),
            };
            if let Some(v) = v {
                columns.insert(col.clone(), v);
            }
        }
        for (name, p) in &sm.facets {
            for v in texts(body, p) {
                facets.insert(format!("{name}/{v}"));
            }
        }
    }

    // Duplicate page numbers would violate the pages primary key; keep the first occurrence.
    let mut seen = BTreeSet::new();
    pages.retain(|p| seen.insert(p.page_no));
    // Offsets follow `content.join("\n")`; `left(content, n)` counts characters.
    let mut offset: usize = content.iter().map(|c| c.chars().count() + 1).sum();
    for p in &mut pages {
        offset += p.text.chars().count();
        p.in_card = offset < CARD_TSV_CHARS;
        offset += 1;
    }
    content.extend(pages.iter().map(|p| p.text.clone()));
    // Text past the indexed prefix is not searchable through the card; full page text is kept in
    // `search.pages`.
    let mut content = content.join("\n");
    if let Some((i, _)) = content.char_indices().nth(CARD_TSV_CHARS) {
        content.truncate(i);
    }

    let card = Card {
        doc_id: src.doc_id,
        filename: src.filename.clone(),
        content,
        fields: Value::Object(fields),
        columns,
        facets: facets.into_iter().collect(),
        schemas: src.artifacts.iter().map(|a| a.schema.clone()).collect(),
        created_at: src.created_at,
        updated_at,
    };
    (card, pages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn mapping() -> Mapping {
        let m: Mapping = toml::from_str(include_str!("../config/mapping.toml")).unwrap();
        m.validate().unwrap();
        m
    }

    fn source() -> DocumentSource {
        DocumentSource {
            doc_id: Uuid::nil(),
            filename: Some("claim-001.pdf".into()),
            created_at: datetime!(2026-09-01 12:00 UTC),
            artifacts: vec![
                DerivedArtifact {
                    schema: "inference.claim.v1".into(),
                    created_at: datetime!(2026-09-02 08:00 UTC),
                    body: serde_json::from_str(include_str!("../../fixtures/sample.claim.json"))
                        .map(Arc::new)
                        .unwrap(),
                },
                DerivedArtifact {
                    schema: "intake.ir.v1".into(),
                    created_at: datetime!(2026-09-01 13:00 UTC),
                    body: serde_json::from_str(include_str!("../../fixtures/sample.ir.json"))
                        .map(Arc::new)
                        .unwrap(),
                },
                DerivedArtifact {
                    schema: "ocr.raw.v1".into(),
                    created_at: datetime!(2026-09-01 14:00 UTC),
                    body: Arc::new(serde_json::json!({"engine": "x"})),
                },
            ],
        }
    }

    #[test]
    fn builds_card_from_fixtures() {
        let (card, pages) = build(&mapping(), &source());
        assert_eq!(
            card.columns,
            BTreeMap::from([
                ("amount".into(), "18450.75".into()),
                ("received_at".into(), "2026-08-28T14:30:00Z".into()),
                ("status".into(), "pending_review".into()),
            ])
        );
        assert_eq!(card.updated_at, datetime!(2026-09-02 08:00 UTC));
        assert_eq!(
            card.schemas,
            vec!["inference.claim.v1", "intake.ir.v1", "ocr.raw.v1"]
        );
        for f in [
            "type/insurance_claim",
            "party_role/claimant",
            "needs_ocr/true",
            "schema/ocr.raw.v1",
        ] {
            assert!(card.facets.contains(&f.to_string()), "{f}");
        }
        assert!(card.content.contains("Property damage claim"));
        assert!(card.content.contains("Jordan Alvarez"));
        assert!(card.content.contains("mold remediation"));
        // Empty page 3 is dropped.
        assert_eq!(
            pages.iter().map(|p| p.page_no).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(pages.iter().all(|p| p.in_card));
        assert!(card.fields["ocr.raw.v1"]["engine"] == "x");
        // `fields = false` in the mapping.
        assert!(card.fields.get("intake.ir.v1").is_none());
    }

    #[test]
    fn unmapped_schema_only() {
        let mut src = source();
        src.artifacts.drain(0..2);
        let (card, pages) = build(&mapping(), &src);
        assert!(card.columns.is_empty() && pages.is_empty() && card.content.is_empty());
        assert_eq!(card.facets, vec!["schema/ocr.raw.v1"]);

        let mut m = mapping();
        m.ignore.push("ocr.raw.v1".into());
        m.validate().unwrap();
        let (card, _) = build(&m, &src);
        assert_eq!(card.facets, vec!["schema/ocr.raw.v1"]);
        assert_eq!(card.fields, serde_json::json!({}));
    }

    #[test]
    fn pages_beyond_indexed_prefix() {
        let mut src = source();
        let text = |c: &str| c.repeat(CARD_TSV_CHARS / 3);
        src.artifacts[1].body = Arc::new(serde_json::json!({"pages": [
            {"number": 1, "text": text("a")},
            {"number": 2, "text": text("b")},
            {"number": 3, "text": text("c")},
        ]}));
        let (card, pages) = build(&mapping(), &src);
        assert_eq!(
            card.content.chars().count(),
            CARD_TSV_CHARS,
            "stored content is truncated"
        );
        let indexed = &card.content;
        assert_eq!(
            pages.iter().map(|p| p.in_card).collect::<Vec<_>>(),
            [true, true, false]
        );
        for p in &pages {
            assert_eq!(
                p.in_card,
                indexed.contains(p.text.as_str()),
                "page {}",
                p.page_no
            );
        }
    }

    #[test]
    fn value_parsing() {
        assert_eq!(
            parse_amount(&serde_json::json!("12.50")).as_deref(),
            Some("12.50")
        );
        assert_eq!(parse_amount(&serde_json::json!("NaN")), None);
        assert_eq!(parse_amount(&serde_json::json!("abc")), None);
        assert_eq!(
            parse_timestamp(&serde_json::json!("2026-01-02")),
            Some(datetime!(2026-01-02 0:00 UTC))
        );
        assert_eq!(parse_timestamp(&serde_json::json!("yesterday")), None);
    }

    #[test]
    fn rejects_bad_config() {
        let bad: Mapping = toml::from_str("[[schema]]\nname = \"x\"\ncontent = [\"$[\"]").unwrap();
        assert!(bad.validate().is_err());
        let bad: Mapping =
            toml::from_str("[[schema]]\nname = \"x\"\n[schema.facets]\nschema = \"$.a\"").unwrap();
        assert!(bad.validate().is_err());
        let bad: Mapping = toml::from_str("ignore = [\"x\"]\n[[schema]]\nname = \"x\"").unwrap();
        assert!(bad.validate().is_err());
        for cols in [
            "[columns]\ncreated_at = { type = \"timestamp\" }",
            "[columns]\nStatus = { type = \"text\" }",
            "[columns]\nstatus = { type = \"text\", sort = true }",
            "[[schema]]\nname = \"x\"\n[schema.columns]\nnope = \"$.a\"",
        ] {
            let bad: Mapping = toml::from_str(cols).unwrap();
            assert!(bad.validate().is_err(), "{cols}");
        }
    }
}

#[cfg(test)]
mod form_tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn form_and_receipt_schemas() {
        let m: Mapping = toml::from_str(include_str!("../config/mapping.toml")).unwrap();
        m.validate().unwrap();
        let src = DocumentSource {
            doc_id: Uuid::nil(),
            filename: None,
            created_at: datetime!(2026-09-01 12:00 UTC),
            artifacts: vec![
                DerivedArtifact {
                    schema: "intake.form.v1".into(),
                    created_at: datetime!(2026-09-01 12:01 UTC),
                    body: Arc::new(
                        serde_json::json!({"form": {"template_id": "uscis.i-765@2025-08-21", "form_type": "uscis.i-765"}, "fields": [
                        {"status": "filled", "value": "Nguyen"}, {"status": "requires_receipting", "value": "CH83"}]}),
                    ),
                },
                DerivedArtifact {
                    schema: "receipting.form.v1".into(),
                    created_at: datetime!(2026-09-01 12:05 UTC),
                    body: Arc::new(
                        serde_json::json!({"form_type": "uscis.i-765", "mode": "review", "values": {"a": "C4839201", "b": "Portland"}}),
                    ),
                },
            ],
        };
        let (card, _) = build(&m, &src);
        assert!(
            card.content.contains("C4839201")
                && card.content.contains("Nguyen")
                && !card.content.contains("CH83")
        );
        assert!(card.facets.contains(&"form/uscis.i-765".to_string()));
        assert!(
            card.facets
                .contains(&"form_edition/uscis.i-765@2025-08-21".to_string())
        );
        assert!(card.facets.contains(&"receipted/review".to_string()));
    }
}

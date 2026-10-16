//! `intake.ir.v1`: the intermediate representation produced from a source PDF.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::extract::{ENGINE, Extraction, Metadata};

pub const SCHEMA: &str = "intake.ir.v1";
/// Pages with fewer non-whitespace chars are treated as having no text layer.
const MIN_TEXT_CHARS: usize = 16;
const MAX_TITLE_CHARS: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IntakeIr {
    pub schema: String,
    pub extractor: Extractor,
    pub source: Source,
    pub metadata: Metadata,
    /// PDF metadata title, else the first non-empty line of text.
    pub title: Option<String>,
    pub pages: Vec<Page>,
    pub stats: Stats,
    pub flags: Flags,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Extractor {
    pub name: String,
    pub version: String,
    pub engine: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Source {
    pub document_id: Uuid,
    pub artifact_id: Uuid,
    pub sha256: String,
    pub filename: Option<String>,
    pub size_bytes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Page {
    pub number: u32,
    pub text: String,
    pub chars: usize,
    pub has_text: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Stats {
    pub pages: u32,
    pub text_pages: u32,
    pub empty_pages: u32,
    pub chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Flags {
    /// At least one page has no usable text layer.
    pub needs_ocr: bool,
    pub empty_pages: Vec<u32>,
    pub failed_pages: Vec<u32>,
}

pub fn build(source: Source, ex: Extraction) -> IntakeIr {
    let pages: Vec<Page> = ex
        .pages
        .into_iter()
        .map(|p| {
            let visible = p.text.chars().filter(|c| !c.is_whitespace()).count();
            Page {
                number: p.number,
                chars: p.text.chars().count(),
                has_text: visible >= MIN_TEXT_CHARS,
                text: p.text,
                error: p.error,
            }
        })
        .collect();
    let empty_pages: Vec<u32> = pages
        .iter()
        .filter(|p| !p.has_text)
        .map(|p| p.number)
        .collect();
    let failed_pages: Vec<u32> = pages
        .iter()
        .filter(|p| p.error.is_some())
        .map(|p| p.number)
        .collect();
    let title = ex.metadata.title.clone().or_else(|| {
        pages
            .iter()
            .filter(|p| p.has_text)
            .flat_map(|p| p.text.lines())
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(|l| l.chars().take(MAX_TITLE_CHARS).collect())
    });
    let stats = Stats {
        pages: pages.len() as u32,
        text_pages: (pages.len() - empty_pages.len()) as u32,
        empty_pages: empty_pages.len() as u32,
        chars: pages.iter().map(|p| p.chars).sum(),
    };
    IntakeIr {
        schema: SCHEMA.into(),
        extractor: Extractor {
            name: crate::SERVICE.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            engine: ENGINE.into(),
        },
        source,
        metadata: ex.metadata,
        title,
        flags: Flags {
            needs_ocr: !empty_pages.is_empty(),
            empty_pages,
            failed_pages,
        },
        stats,
        pages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::extract;

    fn source() -> Source {
        Source {
            document_id: Uuid::nil(),
            artifact_id: Uuid::nil(),
            sha256: "0".repeat(64),
            filename: None,
            size_bytes: 1,
        }
    }

    #[test]
    fn builds_from_multipage_fixture() {
        let ex = extract(include_bytes!("../../fixtures/multipage.pdf"), 10_000).unwrap();
        let ir = build(source(), ex);
        assert_eq!(ir.title.as_deref(), Some("Property damage claim"));
        assert_eq!(
            (ir.stats.pages, ir.stats.text_pages, ir.stats.empty_pages),
            (3, 2, 1)
        );
        assert!(ir.flags.needs_ocr);
        assert_eq!(ir.flags.empty_pages, vec![3]);
        assert!(ir.flags.failed_pages.is_empty());
    }

    #[test]
    fn title_falls_back_to_first_line() {
        let ex = extract(include_bytes!("../../fixtures/sample.pdf"), 10_000).unwrap();
        let ir = build(source(), ex);
        assert!(ir.title.unwrap().starts_with("Property damage claim"));
        assert!(!ir.flags.needs_ocr);
    }

    /// The search fixture must stay a valid instance of this schema.
    #[test]
    fn search_fixture_matches_schema() {
        let _: IntakeIr =
            serde_json::from_str(include_str!("../../fixtures/sample.ir.json")).unwrap();
    }
}

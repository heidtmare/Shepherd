//! PDF text-layer extraction (no OCR).

use std::panic::{AssertUnwindSafe, catch_unwind};

use pdf_extract::{
    Dictionary, Document, Object, PlainTextOutput, decode_text_string, output_doc_page,
};
use serde::{Deserialize, Serialize};
use time::{
    Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset,
    format_description::well_known::Rfc3339,
};

pub const ENGINE: &str = "pdf-extract/0.12";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExtractError {
    #[error("not a readable PDF: {0}")]
    Invalid(String),
    #[error("encrypted PDF requires a password")]
    Encrypted,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub keywords: Option<String>,
    pub creator: Option<String>,
    pub producer: Option<String>,
    /// RFC 3339.
    pub created: Option<String>,
    /// RFC 3339.
    pub modified: Option<String>,
    pub pdf_version: String,
    pub encrypted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageText {
    pub number: u32,
    pub text: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Extraction {
    pub metadata: Metadata,
    pub pages: Vec<PageText>,
}

/// Parses the PDF and extracts per-page text. Failures on individual pages (including extractor
/// panics on malformed content streams) are recorded on the page, not propagated.
pub fn extract(bytes: &[u8], max_page_chars: usize) -> Result<Extraction, ExtractError> {
    let mut doc = catch_unwind(|| Document::load_mem(bytes))
        .map_err(|_| ExtractError::Invalid("parser panicked".into()))?
        .map_err(|e| ExtractError::Invalid(e.to_string()))?;
    let encrypted = doc.is_encrypted();
    if encrypted {
        // Owner-password-only PDFs open with an empty user password.
        doc.decrypt("").map_err(|_| ExtractError::Encrypted)?;
    }
    let mut metadata = read_metadata(&doc);
    metadata.encrypted = encrypted;
    let pages = doc
        .get_pages()
        .keys()
        .map(|&n| page_text(&doc, n, max_page_chars))
        .collect();
    Ok(Extraction { metadata, pages })
}

fn page_text(doc: &Document, number: u32, max_chars: usize) -> PageText {
    let res = catch_unwind(AssertUnwindSafe(|| {
        let mut s = String::new();
        {
            let mut out = PlainTextOutput::new(&mut s);
            output_doc_page(doc, &mut out, number)?;
        }
        Ok::<_, pdf_extract::OutputError>(s)
    }));
    match res {
        Ok(Ok(s)) => PageText {
            number,
            text: normalize(&s, max_chars),
            error: None,
        },
        Ok(Err(e)) => PageText {
            number,
            text: String::new(),
            error: Some(e.to_string()),
        },
        Err(_) => PageText {
            number,
            text: String::new(),
            error: Some("extractor panicked".into()),
        },
    }
}

/// Trims trailing whitespace per line, collapses runs of blank lines, caps length at a char boundary.
pub fn normalize(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars));
    let mut blank_run = 0;
    for line in s.replace('\r', "").lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    let trimmed = out.trim();
    match trimmed.char_indices().nth(max_chars) {
        Some((i, _)) => trimmed[..i].to_owned(),
        None => trimmed.to_owned(),
    }
}

fn info_dict(doc: &Document) -> Option<&Dictionary> {
    let obj = doc.trailer.get(b"Info").ok()?;
    match obj {
        Object::Reference(id) => doc.get_dictionary(*id).ok(),
        Object::Dictionary(d) => Some(d),
        _ => None,
    }
}

fn read_metadata(doc: &Document) -> Metadata {
    let mut m = Metadata {
        pdf_version: doc.version.clone(),
        ..Default::default()
    };
    let Some(info) = info_dict(doc) else { return m };
    let text = |key: &[u8]| {
        info.get(key)
            .ok()
            .and_then(|o| doc.dereference(o).ok().map(|(_, o)| o))
            .and_then(|o| decode_text_string(o).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    m.title = text(b"Title");
    m.author = text(b"Author");
    m.subject = text(b"Subject");
    m.keywords = text(b"Keywords");
    m.creator = text(b"Creator");
    m.producer = text(b"Producer");
    m.created = text(b"CreationDate").as_deref().and_then(parse_pdf_date);
    m.modified = text(b"ModDate").as_deref().and_then(parse_pdf_date);
    m
}

/// Parses a PDF date (`D:YYYYMMDDHHmmSSOHH'mm'`, trailing fields optional) to RFC 3339.
pub fn parse_pdf_date(s: &str) -> Option<String> {
    let s = s.strip_prefix("D:").unwrap_or(s);
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() < 4 {
        return None;
    }
    let num = |from: usize, len: usize, default: u32| -> Option<u32> {
        digits
            .get(from..from + len)
            .map_or(Some(default), |v| v.parse().ok())
    };
    let date = Date::from_calendar_date(
        num(0, 4, 0)? as i32,
        Month::try_from(num(4, 2, 1)? as u8).ok()?,
        num(6, 2, 1)? as u8,
    )
    .ok()?;
    let time = Time::from_hms(
        num(8, 2, 0)? as u8,
        num(10, 2, 0)? as u8,
        num(12, 2, 0)? as u8,
    )
    .ok()?;
    let rest = &s[digits.len()..];
    let offset = match rest.chars().next() {
        Some(sign @ ('+' | '-')) => {
            let tz: String = rest[1..].chars().filter(char::is_ascii_digit).collect();
            let h: i8 = tz.get(0..2)?.parse().ok()?;
            let mi: i8 = tz.get(2..4).map_or(Some(0), |v| v.parse().ok())?;
            let (h, mi) = if sign == '-' { (-h, -mi) } else { (h, mi) };
            UtcOffset::from_hms(h, mi, 0).ok()?
        }
        _ => UtcOffset::UTC,
    };
    let dt: OffsetDateTime = PrimitiveDateTime::new(date, time).assume_offset(offset);
    dt.format(&Rfc3339).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULTIPAGE: &[u8] = include_bytes!("../../fixtures/multipage.pdf");
    const SINGLE: &[u8] = include_bytes!("../../fixtures/sample.pdf");

    #[test]
    fn extracts_pages_and_metadata() {
        let ex = extract(MULTIPAGE, 10_000).unwrap();
        assert_eq!(ex.pages.len(), 3);
        assert!(
            ex.pages[0].text.contains("Policy HX-44821"),
            "{:?}",
            ex.pages[0].text
        );
        assert!(ex.pages[1].text.contains("mold remediation"));
        assert!(ex.pages[2].text.is_empty() && ex.pages[2].error.is_none());
        let m = &ex.metadata;
        assert_eq!(m.title.as_deref(), Some("Property damage claim"));
        assert_eq!(m.author.as_deref(), Some("Northwind Mutual"));
        assert_eq!(m.created.as_deref(), Some("2026-08-28T14:30:00Z"));
        assert_eq!(m.modified.as_deref(), Some("2026-08-29T09:00:00+02:00"));
        assert_eq!(m.pdf_version, "1.7");
        assert!(!m.encrypted);
    }

    #[test]
    fn single_page_without_info() {
        let ex = extract(SINGLE, 10_000).unwrap();
        assert_eq!(ex.pages.len(), 1);
        assert!(ex.pages[0].text.contains("water damage"));
        assert_eq!(ex.metadata.title, None);
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            extract(b"%PDF-1.7 not really", 100),
            Err(ExtractError::Invalid(_))
        ));
    }

    #[test]
    fn normalization() {
        assert_eq!(normalize("  a  \r\n\n\n\nb\t\n", 100), "a\n\nb");
        assert_eq!(normalize("héllo", 2), "hé");
    }

    #[test]
    fn pdf_dates() {
        assert_eq!(
            parse_pdf_date("D:2026").as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert_eq!(
            parse_pdf_date("D:20260102030405-05'30'").as_deref(),
            Some("2026-01-02T03:04:05-05:30")
        );
        assert_eq!(parse_pdf_date("D:20261340"), None);
        assert_eq!(parse_pdf_date("yesterday"), None);
    }
}

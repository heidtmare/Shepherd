//! AcroForm field inventory and values, read with lopdf.
//!
//! USCIS forms are XFA/AcroForm hybrids; the AcroForm widgets carry name (`/T`), tooltip (`/TU`),
//! type (`/FT`), rectangle and page, which is all a template needs.

use std::collections::{BTreeMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId};
use sha2::{Digest, Sha256};

use crate::{
    template::{FieldKind, FieldTemplate, FormTemplate, PageTemplate},
    value,
};

#[derive(Debug, Clone)]
pub struct RawField {
    pub name: String,
    pub tooltip: Option<String>,
    pub field_type: String,
    pub rect: [f32; 4],
    pub page: u32,
    pub max_len: Option<usize>,
    pub options: Vec<String>,
    /// Current value (`/V`): text, choice, or the on-state name for buttons.
    pub value: Option<String>,
    /// Checkbox on-state names from the appearance dictionary.
    pub on_states: Vec<String>,
    /// Widget is hidden or read-only (e.g. barcodes).
    pub skip: bool,
}

fn text(doc: &Document, o: &Object) -> Option<String> {
    let o = doc.dereference(o).ok()?.1;
    match o {
        Object::Name(n) => Some(String::from_utf8_lossy(n).into_owned()),
        _ => lopdf::decode_text_string(o).ok(),
    }
}

fn num(o: &Object) -> Option<f32> {
    o.as_float()
        .ok()
        .or_else(|| o.as_i64().ok().map(|i| i as f32))
}

fn get<'a>(doc: &'a Document, d: &'a Dictionary, key: &[u8]) -> Option<&'a Object> {
    d.get(key)
        .ok()
        .and_then(|o| doc.dereference(o).ok().map(|(_, o)| o))
}

struct Inherited {
    name: String,
    ft: Option<String>,
    tooltip: Option<String>,
    value: Option<String>,
    max_len: Option<usize>,
    options: Vec<String>,
    flags: i64,
}

fn walk(
    doc: &Document,
    id: ObjectId,
    parent: &Inherited,
    pages: &BTreeMap<ObjectId, u32>,
    out: &mut Vec<RawField>,
) {
    let Ok(d) = doc.get_dictionary(id) else {
        return;
    };
    let partial = get(doc, d, b"T").and_then(|o| text(doc, o));
    let name = match (&partial, parent.name.is_empty()) {
        (Some(t), true) => t.clone(),
        (Some(t), false) => format!("{}.{t}", parent.name),
        (None, _) => parent.name.clone(),
    };
    let cur = Inherited {
        name,
        ft: get(doc, d, b"FT")
            .and_then(|o| text(doc, o))
            .or_else(|| parent.ft.clone()),
        tooltip: get(doc, d, b"TU")
            .and_then(|o| text(doc, o))
            .or_else(|| parent.tooltip.clone()),
        value: get(doc, d, b"V")
            .and_then(|o| text(doc, o))
            .or_else(|| parent.value.clone()),
        max_len: get(doc, d, b"MaxLen")
            .and_then(|o| o.as_i64().ok())
            .map(|v| v as usize)
            .or(parent.max_len),
        options: get(doc, d, b"Opt")
            .and_then(|o| o.as_array().ok())
            .map(|a| {
                a.iter()
                    .filter_map(|o| match doc.dereference(o).ok()?.1 {
                        // [export, display] pairs: keep the display string
                        Object::Array(pair) => pair.last().and_then(|x| text(doc, x)),
                        other => text(doc, other),
                    })
                    .collect()
            })
            .unwrap_or_else(|| parent.options.clone()),
        flags: get(doc, d, b"Ff")
            .and_then(|o| o.as_i64().ok())
            .unwrap_or(parent.flags),
    };
    if let Some(kids) = get(doc, d, b"Kids").and_then(|k| k.as_array().ok()) {
        for k in kids {
            if let Ok(kid) = k.as_reference() {
                walk(doc, kid, &cur, pages, out);
            }
        }
        return;
    }
    let Some(rect) = get(doc, d, b"Rect").and_then(|r| r.as_array().ok()) else {
        return;
    };
    let v: Vec<f32> = rect.iter().filter_map(num).collect();
    if v.len() != 4 {
        return;
    }
    let page = d
        .get(b"P")
        .ok()
        .and_then(|p| p.as_reference().ok())
        .and_then(|p| pages.get(&p).copied())
        .unwrap_or(0);
    let annot_flags = get(doc, d, b"F").and_then(|o| o.as_i64().ok()).unwrap_or(0);
    let on_states = get(doc, d, b"AP")
        .and_then(|ap| ap.as_dict().ok())
        .and_then(|ap| get(doc, ap, b"N"))
        .and_then(|n| n.as_dict().ok())
        .map(|n| {
            n.iter()
                .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
                .filter(|k| k != "Off")
                .collect()
        })
        .unwrap_or_default();
    // Annotation flag bit 2 = hidden; field flag bit 1 = read-only.
    let skip = annot_flags & 2 != 0 || cur.flags & 1 != 0 || cur.name.contains("BarCode");
    let (x0, x1) = (v[0].min(v[2]), v[0].max(v[2]));
    let (y0, y1) = (v[1].min(v[3]), v[1].max(v[3]));
    out.push(RawField {
        name: cur.name,
        tooltip: cur.tooltip,
        field_type: cur.ft.unwrap_or_default(),
        rect: [x0, y0, x1, y1],
        page,
        max_len: cur.max_len,
        options: cur.options,
        value: cur.value,
        on_states,
        skip,
    });
}

/// All widget-level fields in document order. Empty when the PDF has no AcroForm.
pub fn read_fields(doc: &Document) -> Vec<RawField> {
    let pages: BTreeMap<ObjectId, u32> =
        doc.get_pages().into_iter().map(|(n, id)| (id, n)).collect();
    let Some(acro) = doc
        .trailer
        .get(b"Root")
        .ok()
        .and_then(|r| r.as_reference().ok())
        .and_then(|r| doc.get_dictionary(r).ok())
        .and_then(|cat| get(doc, cat, b"AcroForm"))
        .and_then(|a| a.as_dict().ok())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let root = Inherited {
        name: String::new(),
        ft: None,
        tooltip: None,
        value: None,
        max_len: None,
        options: vec![],
        flags: 0,
    };
    if let Some(fields) = get(doc, acro, b"Fields").and_then(|f| f.as_array().ok()) {
        for f in fields {
            if let Ok(id) = f.as_reference() {
                walk(doc, id, &root, &pages, &mut out);
            }
        }
    }
    out
}

/// Stable per-widget ids: the field name, suffixed with `#<on-state>` (or index) for widgets
/// sharing a name (radio groups). Computed over all widgets so templates and filled PDFs agree.
pub fn widget_ids(raw: &[RawField]) -> Vec<String> {
    let mut seen = HashSet::new();
    raw.iter()
        .enumerate()
        .map(|(i, f)| {
            let mut id = f.name.clone();
            if !seen.insert(id.clone()) {
                id = format!(
                    "{}#{}",
                    f.name,
                    f.on_states
                        .first()
                        .cloned()
                        .unwrap_or_else(|| i.to_string())
                );
                seen.insert(id.clone());
            }
            id
        })
        .collect()
}

/// Strips the standard USCIS preamble and returns (label, section).
fn clean_label(tooltip: &str) -> (String, Option<String>) {
    let t = tooltip.split_whitespace().collect::<Vec<_>>().join(" ");
    let section = t
        .strip_prefix("Part ")
        .and_then(|rest| rest.split(['.', ' ']).next())
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .map(|n| format!("Part {n}"));
    (t, section)
}

/// Last one or two sentences of the tooltip, minus imperative verbs and `Part N.` headings.
pub fn short_label(label: &str) -> String {
    const BOILERPLATE: &[&str] = &["Complete this form and clip it on top of the first page"];
    let sentences: Vec<&str> = label
        .split(". ")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| !BOILERPLATE.iter().any(|b| s.starts_with(b)))
        .filter(|s| !s.starts_with("Part ") && !s.starts_with("NOTE") && !s.starts_with("Note"))
        .collect();
    // Prefer the sentence carrying the item number ("1. A. Enter ...") and the one after it.
    let start = sentences
        .iter()
        .rposition(|s| s.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or(sentences.len().saturating_sub(1));
    let mut out = sentences
        .get(start..)
        .map(|s| s.join(". "))
        .unwrap_or_default();
    for verb in ["Enter your ", "Enter the ", "Enter ", "Select "] {
        out = out.replacen(verb, "", 1);
    }
    let out = out.trim().trim_end_matches('.').to_owned();
    if out.chars().count() > 90 {
        out.chars().take(87).collect::<String>() + "..."
    } else if out.is_empty() {
        label.to_owned()
    } else {
        out
    }
}

fn group_of(id: &str) -> String {
    let base = id.split('#').next().unwrap_or(id);
    match base.rfind('[') {
        Some(i) if base.ends_with(']') => base[..i].to_owned(),
        _ => base.to_owned(),
    }
}

fn partial_name(name: &str) -> String {
    let last = name.rsplit('.').next().unwrap_or(name);
    last.split('[').next().unwrap_or(last).replace('_', " ")
}

/// Builds a template from a blank fillable PDF.
pub fn template_from_pdf(
    bytes: &[u8],
    id: &str,
    title: &str,
    blank_pdf_rel: &str,
) -> anyhow::Result<FormTemplate> {
    let doc = Document::load_mem(bytes)?;
    let raw = read_fields(&doc);
    anyhow::ensure!(!raw.is_empty(), "PDF has no AcroForm fields");

    let pages = doc
        .get_pages()
        .into_iter()
        .map(|(number, pid)| {
            let mb = doc
                .get_dictionary(pid)
                .ok()
                .and_then(|p| get(&doc, p, b"MediaBox"))
                .and_then(|m| m.as_array().ok())
                .map(|a| a.iter().filter_map(num).collect::<Vec<_>>())
                .filter(|v| v.len() == 4)
                .unwrap_or_else(|| vec![0.0, 0.0, 612.0, 792.0]);
            PageTemplate {
                number,
                width: mb[2] - mb[0],
                height: mb[3] - mb[1],
            }
        })
        .collect();

    let ids = widget_ids(&raw);
    let mut fields = Vec::new();
    for (f, fid) in raw
        .into_iter()
        .zip(ids)
        .filter(|(f, _)| !f.skip && f.page > 0)
    {
        let kind = match f.field_type.as_str() {
            "Tx" => FieldKind::Text,
            "Btn" if !f.on_states.is_empty() => FieldKind::Checkbox,
            "Ch" => FieldKind::Choice,
            "Sig" => FieldKind::Signature,
            _ => continue,
        };
        let (label, section) = match &f.tooltip {
            Some(t) if !t.trim().is_empty() => clean_label(t),
            _ => (partial_name(&f.name), None),
        };
        let value_type = match kind {
            FieldKind::Checkbox => value::ValueType::Checkbox,
            FieldKind::Choice => value::ValueType::Choice,
            FieldKind::Signature => value::ValueType::Signature,
            FieldKind::Text => value::infer(&label, &f.name, f.max_len),
        };
        let group = (kind == FieldKind::Checkbox).then(|| group_of(&fid));
        fields.push(FieldTemplate {
            short_label: short_label(&label),
            group,
            id: fid,
            label,
            section,
            page: f.page,
            rect: f.rect,
            kind,
            value_type,
            max_len: f.max_len.filter(|&m| m > 0),
            options: if kind == FieldKind::Choice {
                f.options
            } else {
                vec![]
            },
            required: false,
        });
    }

    let text = doc.extract_text(&[1]).unwrap_or_default();
    let form_number = find_form_number(&text)
        .unwrap_or_else(|| id.rsplit('.').next().unwrap_or(id).to_uppercase());
    Ok(FormTemplate {
        id: id.to_owned(),
        form_type: None,
        aliases: vec![],
        form_number,
        title: title.to_owned(),
        edition: find_edition(&doc),
        blank_pdf: blank_pdf_rel.to_owned(),
        blank_sha256: hex::encode(Sha256::digest(bytes)),
        pages,
        fields,
    })
}

fn find_form_number(text: &str) -> Option<String> {
    let i = text.find("Form ")?;
    let rest = &text[i + 5..];
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    (token.contains('-') && token.len() <= 8).then_some(token)
}

/// Looks for `Edition MM/DD/YY` in the text of every page.
fn find_edition(doc: &Document) -> Option<String> {
    let n = doc.get_pages().len() as u32;
    for p in 1..=n {
        let t = doc.extract_text(&[p]).unwrap_or_default();
        if let Some(i) = t.find("Edition") {
            let date: String = t[i + 7..].trim_start().chars().take(8).collect();
            if date.len() == 8 && date.chars().filter(|c| *c == '/').count() == 2 {
                return Some(date);
            }
        }
    }
    None
}

/// Filled values keyed by widget id (see [`widget_ids`]), for PDFs that were typed into rather
/// than handwritten. Buttons are `true` when `/V` names that widget's on-state.
pub fn filled_values(doc: &Document) -> BTreeMap<String, String> {
    let raw = read_fields(doc);
    let ids = widget_ids(&raw);
    raw.into_iter()
        .zip(ids)
        .filter_map(|(f, id)| {
            let v = f.value?;
            let v = if f.field_type == "Btn" {
                if !f.on_states.contains(&v) {
                    return None;
                }
                "true".to_owned()
            } else {
                v
            };
            (!v.trim().is_empty()).then_some((id, v))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_labels() {
        assert_eq!(
            short_label(
                "Part 2. Information About You. Your Full Legal Name. 1. A. Enter Family Name, Last Name."
            ),
            "1. A. Family Name, Last Name"
        );
        assert_eq!(
            short_label(
                "Complete this form and clip it on top of the first page of your immigration form or forms. Enter E-mail Address."
            ),
            "E-mail Address"
        );
        assert_eq!(
            group_of("form1[0].Page2[0].Line9_Checkbox[1]"),
            "form1[0].Page2[0].Line9_Checkbox"
        );
    }
}

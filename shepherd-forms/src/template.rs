//! Form templates: the field inventory of a known form, derived from its blank fillable PDF.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::value::ValueType;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FormTemplate {
    /// Stable id of this edition, e.g. `uscis.i-765` or `uscis.i-765@2025-08-21`. Recorded in every
    /// artifact produced with the template, so it must never be reused for a different layout.
    pub id: String,
    /// Form type shared by all editions, e.g. `uscis.i-765`. Defaults to `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form_type: Option<String>,
    /// Former ids of this template (artifacts recorded before a rename still resolve).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub form_number: String,
    pub title: String,
    /// Edition date printed on the form (`MM/DD/YY`), when found.
    pub edition: Option<String>,
    /// Blank PDF, relative to the forms root.
    pub blank_pdf: String,
    pub blank_sha256: String,
    pub pages: Vec<PageTemplate>,
    pub fields: Vec<FieldTemplate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageTemplate {
    pub number: u32,
    /// PDF points.
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Text,
    Checkbox,
    Choice,
    Signature,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FieldTemplate {
    /// Fully qualified AcroForm name; unique within the template.
    pub id: String,
    /// Tooltip text (`/TU`) when present, otherwise the partial name.
    pub label: String,
    /// Label without form-wide boilerplate and section headings, for compact display.
    pub short_label: String,
    /// Checkboxes sharing a group are alternatives (e.g. Female / Male).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// e.g. `Part 2`, parsed from the label.
    pub section: Option<String>,
    pub page: u32,
    /// `[x0, y0, x1, y1]` in PDF points, origin bottom-left.
    pub rect: [f32; 4],
    pub kind: FieldKind,
    pub value_type: ValueType,
    pub max_len: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default)]
    pub required: bool,
}

impl FormTemplate {
    pub fn form_type(&self) -> &str {
        self.form_type.as_deref().unwrap_or(&self.id)
    }

    /// `edition` (`MM/DD/YY` or `MM/DD/YYYY`) as `(year, month, day)`, for ordering editions.
    pub fn edition_date(&self) -> Option<(u16, u8, u8)> {
        let mut it = self.edition.as_deref()?.trim().split('/');
        let m: u8 = it.next()?.parse().ok()?;
        let d: u8 = it.next()?.parse().ok()?;
        let y: u16 = it.next()?.parse().ok()?;
        if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return None;
        }
        Some((if y < 100 { 2000 + y } else { y }, m, d))
    }

    fn answers_to(&self, id: &str) -> bool {
        self.id == id || self.aliases.iter().any(|a| a == id)
    }

    pub fn page(&self, number: u32) -> Option<&PageTemplate> {
        self.pages.iter().find(|p| p.number == number)
    }

    pub fn field(&self, id: &str) -> Option<&FieldTemplate> {
        self.fields.iter().find(|f| f.id == id)
    }
}

/// All templates under `<root>/templates/*.json`.
#[derive(Debug, Clone)]
pub struct TemplateSet {
    pub root: PathBuf,
    pub templates: Vec<FormTemplate>,
}

impl TemplateSet {
    pub fn load(root: impl AsRef<Path>) -> anyhow::Result<Self> {
        let root = root.as_ref().to_path_buf();
        let mut templates = Vec::new();
        let dir = root.join("templates");
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .map_err(|e| anyhow::anyhow!("{}: {e}", dir.display()))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        entries.sort();
        for p in entries {
            let t: FormTemplate = serde_json::from_slice(&std::fs::read(&p)?)
                .map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
            templates.push(t);
        }
        Self::new(root, templates)
    }

    /// Validates template identity:
    /// - ids and aliases are unique across the set
    /// - a form type with several editions has no template whose id is the bare form type, so a
    ///   form type reference is never mistaken for one edition (keep the old id as an alias)
    pub fn new(root: PathBuf, templates: Vec<FormTemplate>) -> anyhow::Result<Self> {
        let mut seen = std::collections::HashSet::new();
        for t in &templates {
            for id in std::iter::once(&t.id).chain(&t.aliases) {
                anyhow::ensure!(!id.is_empty(), "template with empty id");
                anyhow::ensure!(
                    seen.insert(id.as_str()),
                    "duplicate template id or alias {id:?}"
                );
            }
        }
        let set = Self { root, templates };
        for ft in set.form_types() {
            let editions = set.editions(ft);
            if editions.len() > 1 {
                anyhow::ensure!(
                    editions.iter().all(|t| t.id != ft),
                    "form type {ft:?} has {} editions; give each an edition id and keep {ft:?} as an alias",
                    editions.len()
                );
            }
        }
        Ok(set)
    }

    /// Template by id or alias.
    pub fn get(&self, id: &str) -> Option<&FormTemplate> {
        self.templates.iter().find(|t| t.answers_to(id))
    }

    /// Distinct form types, in template order.
    pub fn form_types(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for t in &self.templates {
            if !out.contains(&t.form_type()) {
                out.push(t.form_type());
            }
        }
        out
    }

    pub fn is_form_type(&self, s: &str) -> bool {
        self.templates.iter().any(|t| t.form_type() == s)
    }

    /// Editions of a form type, newest first (undated last, then by id).
    pub fn editions(&self, form_type: &str) -> Vec<&FormTemplate> {
        let mut out: Vec<&FormTemplate> = self
            .templates
            .iter()
            .filter(|t| t.form_type() == form_type)
            .collect();
        out.sort_by(|a, b| {
            b.edition_date()
                .cmp(&a.edition_date())
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    pub fn latest(&self, form_type: &str) -> Option<&FormTemplate> {
        self.editions(form_type).into_iter().next()
    }

    /// Templates a reference may denote: every edition of a form type, else the template with that
    /// id or alias. Empty when unknown.
    pub fn candidates(&self, reference: &str) -> Vec<&FormTemplate> {
        if self.is_form_type(reference) {
            self.editions(reference)
        } else {
            self.get(reference).into_iter().collect()
        }
    }

    /// A form type's newest edition, else the template with that id or alias.
    pub fn resolve(&self, reference: &str) -> Option<&FormTemplate> {
        self.candidates(reference).into_iter().next()
    }

    pub fn blank_path(&self, t: &FormTemplate) -> PathBuf {
        self.root.join(&t.blank_pdf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(
        id: &str,
        form_type: Option<&str>,
        edition: Option<&str>,
        aliases: &[&str],
    ) -> FormTemplate {
        FormTemplate {
            id: id.into(),
            form_type: form_type.map(Into::into),
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
            form_number: "I-1".into(),
            title: "t".into(),
            edition: edition.map(Into::into),
            blank_pdf: "x.pdf".into(),
            blank_sha256: String::new(),
            pages: vec![],
            fields: vec![],
        }
    }

    fn set(ts: Vec<FormTemplate>) -> anyhow::Result<TemplateSet> {
        TemplateSet::new(PathBuf::new(), ts)
    }

    #[test]
    fn single_edition_is_its_own_form_type() {
        let s = set(vec![t("a.i-1", None, Some("08/21/25"), &[])]).unwrap();
        assert_eq!(s.form_types(), vec!["a.i-1"]);
        assert_eq!(s.resolve("a.i-1").unwrap().id, "a.i-1");
        assert_eq!(s.candidates("nope").len(), 0);
    }

    #[test]
    fn editions_ordered_and_resolved() {
        let s = set(vec![
            t("a.i-1@2025", Some("a.i-1"), Some("08/21/25"), &["a.i-1"]),
            t("a.i-1@2026", Some("a.i-1"), Some("01/20/2026"), &[]),
            t("a.i-1@old", Some("a.i-1"), None, &[]),
        ])
        .unwrap();
        let ids: Vec<&str> = s.editions("a.i-1").iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["a.i-1@2026", "a.i-1@2025", "a.i-1@old"]);
        // The form type wins over the legacy alias for references; `get` still finds the alias.
        assert_eq!(s.candidates("a.i-1").len(), 3);
        assert_eq!(s.resolve("a.i-1").unwrap().id, "a.i-1@2026");
        assert_eq!(s.get("a.i-1").unwrap().id, "a.i-1@2025");
        assert_eq!(s.resolve("a.i-1@2025").unwrap().id, "a.i-1@2025");
    }

    #[test]
    fn rejects_ambiguous_identity() {
        assert!(set(vec![t("x", None, None, &[]), t("x", None, None, &[])]).is_err());
        assert!(set(vec![t("x", None, None, &[]), t("y", None, None, &["x"])]).is_err());
        // Two editions, one still using the bare form type as its id.
        assert!(
            set(vec![
                t("a.i-1", None, Some("08/21/25"), &[]),
                t("a.i-1@2026", Some("a.i-1"), Some("01/20/26"), &[]),
            ])
            .is_err()
        );
    }

    #[test]
    fn repository_templates_load() {
        let s = TemplateSet::load(concat!(env!("CARGO_MANIFEST_DIR"), "/../forms")).unwrap();
        for ft in s.form_types() {
            assert!(s.latest(ft).is_some());
        }
    }
}

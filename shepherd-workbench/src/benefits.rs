//! Benefit definitions (`config/benefits.toml`): which form types open cases, how case facts are
//! read from form values, and what an officer assesses before deciding.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use shepherd_forms::{
    template::{FormTemplate, TemplateSet},
    value::{self, ValueType},
};
use utoipa::ToSchema;

/// Case fact keys and the value type each is validated with.
pub const FACTS: &[(&str, ValueType)] = &[
    ("family_name", ValueType::Name),
    ("given_name", ValueType::Name),
    ("a_number", ValueType::ANumber),
    ("date_of_birth", ValueType::Date),
    ("beneficiary_family_name", ValueType::Name),
    ("beneficiary_given_name", ValueType::Name),
    ("beneficiary_a_number", ValueType::ANumber),
    ("beneficiary_date_of_birth", ValueType::Date),
    ("mailing_in_care_of", ValueType::Text),
    ("mailing_street", ValueType::Text),
    ("mailing_unit", ValueType::Text),
    ("mailing_city", ValueType::Text),
    ("mailing_state", ValueType::State),
    ("mailing_zip", ValueType::Zip),
    ("mailing_province", ValueType::Text),
    ("mailing_postal_code", ValueType::Text),
    ("mailing_country", ValueType::Text),
];

pub fn fact_type(key: &str) -> Option<ValueType> {
    FACTS.iter().find(|(k, _)| *k == key).map(|(_, t)| *t)
}

/// Normalizes a fact value; empty input normalizes to `""`.
pub fn normalize_fact(key: &str, raw: &str) -> Result<String, String> {
    let t = fact_type(key).ok_or_else(|| format!("unknown fact {key:?}"))?;
    value::normalize(t, raw, None, &[])
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Ground {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Criterion {
    pub id: String,
    pub label: String,
    /// Must be `met` to approve; otherwise `met` or `na`.
    #[serde(default = "yes")]
    pub required: bool,
}

fn yes() -> bool {
    true
}

/// A form field across editions: one field id, or candidates tried in order against the
/// edition's template (the first that the template has wins).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum FieldRef {
    One(String),
    Any(Vec<String>),
}

impl FieldRef {
    pub fn candidates(&self) -> &[String] {
        match self {
            FieldRef::One(f) => std::slice::from_ref(f),
            FieldRef::Any(fs) => fs,
        }
    }

    /// The field id in `template`; without a template (values of an unknown edition), the
    /// first candidate present in `values`.
    pub fn resolve<'a>(
        &'a self,
        template: Option<&FormTemplate>,
        values: &BTreeMap<String, String>,
    ) -> Option<&'a str> {
        self.candidates()
            .iter()
            .find(|f| match template {
                Some(t) => t.field(f).is_some(),
                None => values.contains_key(*f),
            })
            .map(String::as_str)
    }
}

impl From<&str> for FieldRef {
    fn from(s: &str) -> Self {
        FieldRef::One(s.to_owned())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Highlight {
    pub label: String,
    /// Field ids, across editions; the non-empty values present are joined.
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Benefit {
    pub form_type: String,
    pub name: String,
    /// Fact key → field, resolved per edition.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub facts: BTreeMap<String, FieldRef>,
    #[serde(default)]
    pub highlights: Vec<Highlight>,
    pub criteria: Vec<Criterion>,
}

impl Benefit {
    pub fn criterion(&self, id: &str) -> Option<&Criterion> {
        self.criteria.iter().find(|c| c.id == id)
    }

    /// Facts read from final form values (field id → value) recorded with `template`.
    /// Missing values are omitted.
    pub fn derive_facts(
        &self,
        template: Option<&FormTemplate>,
        values: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        self.facts
            .iter()
            .filter_map(|(key, field)| {
                let v = values.get(field.resolve(template, values)?)?.trim();
                let n = normalize_fact(key, v).unwrap_or_else(|_| v.to_owned());
                (!n.is_empty()).then(|| (key.clone(), n))
            })
            .collect()
    }

    pub fn highlight_values(&self, values: &BTreeMap<String, String>) -> Vec<(String, String)> {
        self.highlights
            .iter()
            .filter_map(|h| {
                let joined = h
                    .fields
                    .iter()
                    .filter_map(|f| values.get(f))
                    .map(String::as_str)
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<_>>()
                    .join("");
                (!joined.is_empty()).then(|| (h.label.clone(), joined))
            })
            .collect()
    }
}

/// Form requesting electronic notification of acceptance (G-1145), filed with a benefit form.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ENotification {
    pub form_type: String,
    /// E-mail address field.
    #[schema(value_type = Object)]
    pub email: FieldRef,
    /// Mobile phone number field.
    #[schema(value_type = Option<Object>)]
    pub mobile_phone: Option<FieldRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Benefits {
    pub grounds: Vec<Ground>,
    #[serde(rename = "benefit")]
    pub benefits: Vec<Benefit>,
    #[serde(default)]
    pub e_notification: Option<ENotification>,
}

impl Benefits {
    pub fn load(path: &str, templates: &TemplateSet) -> anyhow::Result<Self> {
        let b: Benefits = toml::from_str(&std::fs::read_to_string(path)?)
            .map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
        b.validate(templates)?;
        Ok(b)
    }

    pub fn get(&self, form_type: &str) -> Option<&Benefit> {
        self.benefits.iter().find(|b| b.form_type == form_type)
    }

    pub fn ground(&self, id: &str) -> Option<&Ground> {
        self.grounds.iter().find(|g| g.id == id)
    }

    /// Every field reference must resolve in every edition of its form type, so adding an edition
    /// whose field ids changed fails at startup until the new ids are listed as candidates.
    fn validate(&self, templates: &TemplateSet) -> anyhow::Result<()> {
        let unique = |ids: Vec<&str>, what: &str| -> anyhow::Result<()> {
            let mut seen = HashSet::new();
            for id in ids {
                anyhow::ensure!(!id.is_empty(), "empty {what} id");
                anyhow::ensure!(seen.insert(id), "duplicate {what} {id:?}");
            }
            Ok(())
        };
        let editions = |form_type: &str, what: &str| -> anyhow::Result<Vec<&FormTemplate>> {
            let e = templates.editions(form_type);
            anyhow::ensure!(!e.is_empty(), "{what} {form_type:?}: no such form type");
            Ok(e)
        };
        let resolves =
            |f: &FieldRef, t: &FormTemplate| f.resolve(Some(t), &BTreeMap::new()).is_some();
        unique(
            self.grounds.iter().map(|g| g.id.as_str()).collect(),
            "ground",
        )?;
        unique(
            self.benefits.iter().map(|b| b.form_type.as_str()).collect(),
            "benefit",
        )?;
        if let Some(en) = &self.e_notification {
            for t in editions(&en.form_type, "e_notification")? {
                for f in std::iter::once(&en.email).chain(&en.mobile_phone) {
                    anyhow::ensure!(
                        resolves(f, t),
                        "e_notification field {:?} not in template {:?}",
                        f.candidates(),
                        t.id
                    );
                }
            }
        }
        for b in &self.benefits {
            let editions = editions(&b.form_type, "benefit")?;
            anyhow::ensure!(
                !b.criteria.is_empty(),
                "benefit {:?}: no criteria",
                b.form_type
            );
            unique(
                b.criteria.iter().map(|c| c.id.as_str()).collect(),
                "criterion",
            )?;
            for key in b.facts.keys() {
                anyhow::ensure!(
                    fact_type(key).is_some(),
                    "benefit {:?}: unknown fact {key:?}",
                    b.form_type
                );
            }
            for t in &editions {
                for (key, field) in &b.facts {
                    anyhow::ensure!(
                        resolves(field, t),
                        "benefit {:?}: fact {key:?} field {:?} not in template {:?}",
                        b.form_type,
                        field.candidates(),
                        t.id
                    );
                }
                for h in &b.highlights {
                    anyhow::ensure!(
                        h.fields.iter().any(|f| t.field(f).is_some()),
                        "benefit {:?}: highlight {:?} has no field in template {:?}",
                        b.form_type,
                        h.label,
                        t.id
                    );
                }
            }
            for h in &b.highlights {
                for f in &h.fields {
                    anyhow::ensure!(
                        editions.iter().any(|t| t.field(f).is_some()),
                        "benefit {:?}: highlight field {f:?} not in any edition",
                        b.form_type
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn shipped_config_matches_templates() {
        let set = TemplateSet::load(root().join("../forms")).unwrap();
        let b =
            Benefits::load(root().join("config/benefits.toml").to_str().unwrap(), &set).unwrap();
        assert!(b.get("uscis.i-765").is_some());
        assert!(b.get("uscis.g-1145").is_none());
        assert_eq!(
            b.e_notification.as_ref().map(|e| e.form_type.as_str()),
            Some("uscis.g-1145")
        );
        let i765 = b.get("uscis.i-765").unwrap();
        assert!(i765.facts.contains_key("mailing_street"));
        assert!(b.ground("abandonment").is_some());
    }

    #[test]
    fn derive_facts_normalizes() {
        let b = Benefit {
            form_type: "t".into(),
            name: "T".into(),
            facts: BTreeMap::from([
                ("family_name".into(), "f1".into()),
                ("a_number".into(), "f2".into()),
                ("date_of_birth".into(), "f3".into()),
            ]),
            highlights: vec![Highlight {
                label: "Category".into(),
                fields: vec!["c1".into(), "c2".into(), "c3".into()],
            }],
            criteria: vec![],
        };
        let values = BTreeMap::from([
            ("f1".to_string(), "Nguyen".to_string()),
            ("f2".to_string(), "A-012-345-678".to_string()),
            ("c1".to_string(), "c".to_string()),
            ("c2".to_string(), "08".to_string()),
        ]);
        let f = b.derive_facts(None, &values);
        assert_eq!(f.get("family_name").map(String::as_str), Some("Nguyen"));
        assert_eq!(f.get("a_number").map(String::as_str), Some("012345678"));
        assert!(!f.contains_key("date_of_birth"));
        assert_eq!(
            b.highlight_values(&values),
            vec![("Category".to_string(), "c08".to_string())]
        );
    }

    #[test]
    fn validation_rejects_unknown_fields() {
        let bad: Benefits = toml::from_str(
            r#"
            grounds = []
            [[benefit]]
            form_type = "uscis.g-1145"
            name = "x"
            facts = { family_name = "nope" }
            [[benefit.criteria]]
            id = "a"
            label = "A"
            "#,
        )
        .unwrap();
        let set = TemplateSet::load(root().join("../forms")).unwrap();
        assert!(bad.validate(&set).is_err());
    }

    fn edition(id: &str, fields: &[&str]) -> FormTemplate {
        use shepherd_forms::template::{FieldKind, FieldTemplate};
        FormTemplate {
            id: id.into(),
            form_type: Some("t".into()),
            aliases: vec![],
            form_number: "T-1".into(),
            title: "T".into(),
            edition: None,
            blank_pdf: String::new(),
            blank_sha256: String::new(),
            pages: vec![],
            fields: fields
                .iter()
                .map(|f| FieldTemplate {
                    id: (*f).into(),
                    label: String::new(),
                    short_label: String::new(),
                    group: None,
                    section: None,
                    page: 1,
                    rect: [0.0; 4],
                    kind: FieldKind::Text,
                    value_type: ValueType::Text,
                    max_len: None,
                    options: vec![],
                    required: false,
                })
                .collect(),
        }
    }

    fn two_editions() -> TemplateSet {
        TemplateSet::new(
            std::path::PathBuf::new(),
            vec![
                edition("t@1", &["name_v1", "email", "cat"]),
                edition("t@2", &["name_v2", "email_v2", "cat"]),
            ],
        )
        .unwrap()
    }

    fn config(family_name: &str, email: &str) -> Benefits {
        toml::from_str(&format!(
            r#"
            grounds = []
            [e_notification]
            form_type = "t"
            email = {email}
            [[benefit]]
            form_type = "t"
            name = "T"
            facts = {{ family_name = {family_name} }}
            highlights = [{{ label = "Category", fields = ["cat"] }}]
            [[benefit.criteria]]
            id = "a"
            label = "A"
            "#
        ))
        .unwrap()
    }

    #[test]
    fn field_refs_resolve_per_edition() {
        let set = two_editions();
        // A single id valid in one edition only fails, naming the edition.
        let err = config(r#""name_v1""#, r#"["email_v2", "email"]"#)
            .validate(&set)
            .unwrap_err()
            .to_string();
        assert!(err.contains("t@2"), "{err}");
        let err = config(r#"["name_v2", "name_v1"]"#, r#""email""#)
            .validate(&set)
            .unwrap_err()
            .to_string();
        assert!(err.contains("e_notification"), "{err}");

        let b = config(r#"["name_v2", "name_v1"]"#, r#"["email_v2", "email"]"#);
        b.validate(&set).unwrap();
        let t = b.get("t").unwrap();
        let values = BTreeMap::from([
            ("name_v1".to_string(), "Old".to_string()),
            ("name_v2".to_string(), "New".to_string()),
        ]);
        let fact = |tid: Option<&str>| {
            t.derive_facts(tid.and_then(|id| set.get(id)), &values)
                .get("family_name")
                .cloned()
        };
        assert_eq!(fact(Some("t@1")).as_deref(), Some("Old"));
        assert_eq!(fact(Some("t@2")).as_deref(), Some("New"));
        // Unknown edition: first candidate present in the values.
        assert_eq!(fact(None).as_deref(), Some("New"));
    }
}

//! `receipting.form.v1`: the human-verified values of a form, with per-field provenance.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use shepherd_forms::{
    recognize::{FieldResult, FieldStatus},
    template::FormTemplate,
    value,
};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

pub const SCHEMA: &str = "receipting.form.v1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Resolve flagged fields; unflagged machine values are kept unless edited.
    Review,
    /// Every field entered by a person; machine values are not used.
    Full,
    /// Correction of a stored receipt (or of a recognition that needed no receipting).
    Amend,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Review => "review",
            Mode::Full => "full",
            Mode::Amend => "amend",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Machine value accepted without review.
    Machine,
    /// Reviewer confirmed the machine value.
    Confirmed,
    /// Reviewer entered or changed the value.
    Human,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct ReceiptField {
    pub id: String,
    pub label: String,
    pub value: Option<String>,
    pub provenance: Provenance,
    pub machine_value: Option<String>,
    #[schema(value_type = Option<String>)]
    pub machine_status: Option<FieldStatus>,
    pub machine_confidence: Option<f32>,
    /// Final value differs from the machine value.
    pub changed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct ReceiptStats {
    pub total: usize,
    pub filled: usize,
    pub machine: usize,
    pub confirmed: usize,
    pub human: usize,
    pub changed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct Receipt {
    pub schema: String,
    pub document_id: Uuid,
    pub task_id: Uuid,
    /// Form type (edition-independent).
    pub form_type: String,
    /// Template (edition) the values were entered against. Absent in receipts recorded before
    /// editions; use [`Receipt::template_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    pub form_number: String,
    pub edition: Option<String>,
    pub mode: Mode,
    pub receipted_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub receipted_at: OffsetDateTime,
    pub source_artifact_id: Uuid,
    pub form_artifact_id: Option<Uuid>,
    pub fields: Vec<ReceiptField>,
    /// Final non-empty values by field id.
    pub values: BTreeMap<String, String>,
    pub stats: ReceiptStats,
    /// Receipt artifact this one corrects (`amend` mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amends: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amendment_reason: Option<String>,
}

impl Receipt {
    pub fn template_id(&self) -> &str {
        self.template_id.as_deref().unwrap_or(&self.form_type)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Rejection {
    pub message: String,
    /// Field id → problem.
    pub fields: BTreeMap<String, String>,
}

pub struct Submission<'a> {
    pub template: &'a FormTemplate,
    /// Machine results by field id (empty for manual tasks without recognition).
    pub machine: &'a HashMap<String, FieldResult>,
    pub mode: Mode,
    /// Reviewer input: field id → value (`None` = confirmed blank). Omitted = untouched.
    pub values: &'a BTreeMap<String, Option<String>>,
}

/// Validates the submission and builds the final field list, or explains what must be fixed.
pub fn resolve(s: &Submission<'_>) -> Result<(Vec<ReceiptField>, ReceiptStats), Rejection> {
    let mut problems = BTreeMap::new();
    for id in s.values.keys() {
        if s.template.field(id).is_none() {
            problems.insert(id.clone(), "not a field of this form".into());
        }
    }
    let mut fields = Vec::with_capacity(s.template.fields.len());
    for tf in &s.template.fields {
        let m = s.machine.get(&tf.id);
        let machine_value = m
            .filter(|m| m.status == FieldStatus::Filled)
            .and_then(|m| m.value.clone());
        let entered = s.values.get(&tf.id);
        let (value, provenance) = match (s.mode, entered) {
            (_, Some(v)) => {
                let raw = v.as_deref().unwrap_or("");
                match value::normalize(tf.value_type, raw, tf.max_len, &tf.options) {
                    Ok(n) => {
                        let unchecked = tf.value_type == value::ValueType::Checkbox && n == "false";
                        let n = (!(n.is_empty() || unchecked)).then_some(n);
                        let machine_reading = m.and_then(|m| m.value.clone());
                        let prov = if s.mode == Mode::Review && n.is_some() && n == machine_reading
                        {
                            Provenance::Confirmed
                        } else {
                            Provenance::Human
                        };
                        (n, prov)
                    }
                    Err(e) => {
                        problems.insert(tf.id.clone(), e);
                        (v.clone(), Provenance::Human)
                    }
                }
            }
            (Mode::Full, None) => {
                problems.insert(
                    tf.id.clone(),
                    "full receipting requires every field (blank allowed)".into(),
                );
                (None, Provenance::Human)
            }
            (Mode::Review | Mode::Amend, None) => {
                if m.is_some_and(|m| m.status == FieldStatus::RequiresReceipting) {
                    problems.insert(tf.id.clone(), "requires receipting".into());
                }
                (machine_value.clone(), Provenance::Machine)
            }
        };
        let changed = provenance == Provenance::Human && value != machine_value;
        fields.push(ReceiptField {
            id: tf.id.clone(),
            label: tf.short_label.clone(),
            value,
            provenance,
            machine_value: m.and_then(|m| m.value.clone()),
            machine_status: m.map(|m| m.status),
            machine_confidence: m.and_then(|m| m.confidence),
            changed,
        });
    }
    if !problems.is_empty() {
        let unresolved = problems
            .values()
            .filter(|p| *p == "requires receipting")
            .count();
        let message = if unresolved == problems.len() {
            format!("{unresolved} flagged field(s) still require receipting")
        } else {
            format!("{} field(s) need attention", problems.len())
        };
        return Err(Rejection {
            message,
            fields: problems,
        });
    }
    let stats = stats(&fields);
    Ok((fields, stats))
}

pub fn stats(fields: &[ReceiptField]) -> ReceiptStats {
    let mut stats = ReceiptStats {
        total: fields.len(),
        ..Default::default()
    };
    for f in fields {
        if f.value.is_some() {
            stats.filled += 1;
            match f.provenance {
                Provenance::Machine => stats.machine += 1,
                Provenance::Confirmed => stats.confirmed += 1,
                Provenance::Human => stats.human += 1,
            }
        } else if f.provenance == Provenance::Human {
            stats.human += 1;
        }
        if f.changed {
            stats.changed += 1;
        }
    }
    stats
}

/// Applies corrections to a baseline field list (a stored receipt's fields, or machine values
/// resolved in review mode). Changed fields become `human`; others keep their provenance.
pub fn amend(
    template: &FormTemplate,
    baseline: &[ReceiptField],
    changes: &BTreeMap<String, Option<String>>,
) -> Result<(Vec<ReceiptField>, ReceiptStats, usize), Rejection> {
    let mut problems = BTreeMap::new();
    let mut fields = baseline.to_vec();
    let mut edited = 0;
    for (id, raw) in changes {
        let Some(tf) = template.field(id) else {
            problems.insert(id.clone(), "not a field of this form".into());
            continue;
        };
        let Some(f) = fields.iter_mut().find(|f| &f.id == id) else {
            problems.insert(id.clone(), "not in the receipt".into());
            continue;
        };
        let n = match value::normalize(
            tf.value_type,
            raw.as_deref().unwrap_or(""),
            tf.max_len,
            &tf.options,
        ) {
            Ok(n) => n,
            Err(e) => {
                problems.insert(id.clone(), e);
                continue;
            }
        };
        let unchecked = tf.value_type == value::ValueType::Checkbox && n == "false";
        let n = (!(n.is_empty() || unchecked)).then_some(n);
        if n == f.value {
            continue;
        }
        edited += 1;
        f.value = n;
        f.provenance = Provenance::Human;
        let machine_value = f
            .machine_value
            .clone()
            .filter(|_| f.machine_status == Some(FieldStatus::Filled));
        f.changed = f.value != machine_value;
    }
    if !problems.is_empty() {
        return Err(Rejection {
            message: format!("{} field(s) need attention", problems.len()),
            fields: problems,
        });
    }
    if edited == 0 {
        return Err(Rejection {
            message: "no values changed".into(),
            fields: BTreeMap::new(),
        });
    }
    let stats = stats(&fields);
    Ok((fields, stats, edited))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepherd_forms::{
        recognize::{Reason, ValueSource},
        template::{FieldKind, FieldTemplate, PageTemplate},
        value::ValueType,
    };

    fn field(id: &str, value_type: ValueType, kind: FieldKind) -> FieldTemplate {
        FieldTemplate {
            id: id.into(),
            label: id.into(),
            short_label: id.into(),
            group: None,
            section: None,
            page: 1,
            rect: [0.0; 4],
            kind,
            value_type,
            max_len: None,
            options: vec![],
            required: false,
        }
    }

    fn template() -> FormTemplate {
        FormTemplate {
            id: "t".into(),
            form_type: None,
            aliases: vec![],
            form_number: "T-1".into(),
            title: "T".into(),
            edition: None,
            blank_pdf: String::new(),
            blank_sha256: String::new(),
            pages: vec![PageTemplate {
                number: 1,
                width: 612.0,
                height: 792.0,
            }],
            fields: vec![
                field("name", ValueType::Name, FieldKind::Text),
                field("dob", ValueType::Date, FieldKind::Text),
                field("phone", ValueType::Phone, FieldKind::Text),
                field("married", ValueType::Checkbox, FieldKind::Checkbox),
            ],
        }
    }

    fn machine() -> HashMap<String, FieldResult> {
        let r = |id: &str, value: Option<&str>, status| FieldResult {
            id: id.into(),
            page: 1,
            value: value.map(str::to_owned),
            raw: value.map(str::to_owned),
            confidence: Some(0.9),
            min_token_prob: None,
            status,
            reasons: if status == FieldStatus::RequiresReceipting {
                vec![Reason {
                    code: "low_confidence".into(),
                    message: String::new(),
                }]
            } else {
                vec![]
            },
            source: Some(ValueSource::Htr),
            ink_bbox: None,
        };
        [
            r("name", Some("Maria"), FieldStatus::Filled),
            r("dob", Some("03/14/1991"), FieldStatus::RequiresReceipting),
            r("phone", None, FieldStatus::Empty),
            r("married", Some("true"), FieldStatus::Filled),
        ]
        .into_iter()
        .map(|f| (f.id.clone(), f))
        .collect()
    }

    #[test]
    fn review_requires_flagged_fields() {
        let (t, m) = (template(), machine());
        let values = BTreeMap::new();
        let err = resolve(&Submission {
            template: &t,
            machine: &m,
            mode: Mode::Review,
            values: &values,
        })
        .unwrap_err();
        assert_eq!(err.fields.keys().collect::<Vec<_>>(), vec!["dob"]);
        assert!(err.message.contains("require receipting"));
    }

    #[test]
    fn review_confirm_edit_and_keep() {
        let (t, m) = (template(), machine());
        let values = BTreeMap::from([
            ("dob".to_string(), Some("3/14/1991".to_string())),
            ("name".to_string(), Some("María".to_string())),
        ]);
        let (fields, stats) = resolve(&Submission {
            template: &t,
            machine: &m,
            mode: Mode::Review,
            values: &values,
        })
        .unwrap();
        let get = |id: &str| fields.iter().find(|f| f.id == id).unwrap();
        assert_eq!(
            (get("dob").value.as_deref(), get("dob").provenance),
            (Some("03/14/1991"), Provenance::Confirmed)
        );
        assert_eq!(
            (get("name").provenance, get("name").changed),
            (Provenance::Human, true)
        );
        assert_eq!(
            (get("married").value.as_deref(), get("married").provenance),
            (Some("true"), Provenance::Machine)
        );
        assert_eq!(get("phone").value, None);
        assert_eq!(
            (stats.machine, stats.confirmed, stats.human, stats.changed),
            (1, 1, 1, 1)
        );
    }

    #[test]
    fn invalid_values_are_rejected() {
        let (t, m) = (template(), machine());
        let values = BTreeMap::from([
            ("dob".to_string(), Some("02/30/1991".to_string())),
            ("phone".to_string(), Some("555".to_string())),
        ]);
        let err = resolve(&Submission {
            template: &t,
            machine: &m,
            mode: Mode::Review,
            values: &values,
        })
        .unwrap_err();
        assert_eq!(err.fields.len(), 2);
    }

    #[test]
    fn full_mode_requires_every_field() {
        let t = template();
        let none = HashMap::new();
        let partial = BTreeMap::from([("name".to_string(), Some("Ana".to_string()))]);
        let err = resolve(&Submission {
            template: &t,
            machine: &none,
            mode: Mode::Full,
            values: &partial,
        })
        .unwrap_err();
        assert_eq!(err.fields.len(), 3);
        let all = BTreeMap::from([
            ("name".to_string(), Some("Ana".to_string())),
            ("dob".to_string(), Some("01/02/1990".to_string())),
            ("phone".to_string(), None),
            ("married".to_string(), Some("false".to_string())),
        ]);
        let (fields, stats) = resolve(&Submission {
            template: &t,
            machine: &none,
            mode: Mode::Full,
            values: &all,
        })
        .unwrap();
        assert!(fields.iter().all(|f| f.provenance == Provenance::Human));
        assert_eq!(stats.filled, 2);
        assert_eq!(
            fields.iter().find(|f| f.id == "married").unwrap().value,
            None
        );
    }

    #[test]
    fn amend_changes_only_edited_fields() {
        let (t, m) = (template(), machine());
        let values = BTreeMap::from([("dob".to_string(), Some("03/14/1991".to_string()))]);
        let (base, _) = resolve(&Submission {
            template: &t,
            machine: &m,
            mode: Mode::Review,
            values: &values,
        })
        .unwrap();
        let changes = BTreeMap::from([
            ("name".to_string(), Some("Mariah".to_string())),
            ("dob".to_string(), Some("3/14/1991".to_string())),
        ]);
        let (fields, stats, edited) = amend(&t, &base, &changes).unwrap();
        assert_eq!(edited, 1);
        let get = |id: &str| fields.iter().find(|f| f.id == id).unwrap();
        assert_eq!(
            (get("name").provenance, get("name").changed),
            (Provenance::Human, true)
        );
        assert_eq!(get("dob").provenance, Provenance::Confirmed);
        assert_eq!(get("married").provenance, Provenance::Machine);
        assert_eq!((stats.human, stats.changed), (1, 1));

        // Reverting to the machine reading is a human value that no longer differs.
        let back = BTreeMap::from([("name".to_string(), Some("Maria".to_string()))]);
        let (fields, _, _) = amend(&t, &fields, &back).unwrap();
        let name = fields.iter().find(|f| f.id == "name").unwrap();
        assert_eq!((name.provenance, name.changed), (Provenance::Human, false));

        assert_eq!(
            amend(&t, &fields, &back).unwrap_err().message,
            "no values changed"
        );
        let bad = BTreeMap::from([
            ("dob".to_string(), Some("13/45/1991".to_string())),
            ("bogus".to_string(), None),
        ]);
        assert_eq!(amend(&t, &fields, &bad).unwrap_err().fields.len(), 2);
    }

    #[test]
    fn unknown_field_ids_are_rejected() {
        let (t, m) = (template(), machine());
        let values = BTreeMap::from([
            ("dob".to_string(), Some("03/14/1991".to_string())),
            ("bogus".to_string(), None),
        ]);
        let err = resolve(&Submission {
            template: &t,
            machine: &m,
            mode: Mode::Review,
            values: &values,
        })
        .unwrap_err();
        assert!(err.fields.contains_key("bogus"));
    }

    #[test]
    fn legacy_receipt_template_is_form_type() {
        let r: Receipt = serde_json::from_value(serde_json::json!({
            "schema": SCHEMA, "document_id": Uuid::nil(), "task_id": Uuid::nil(),
            "form_type": "uscis.i-765", "form_number": "I-765", "edition": null, "mode": "review",
            "receipted_by": "a", "receipted_at": "2026-01-01T00:00:00Z",
            "source_artifact_id": Uuid::nil(), "form_artifact_id": null,
            "fields": [], "values": {}, "stats": ReceiptStats::default(),
        }))
        .unwrap();
        assert_eq!(r.template_id(), "uscis.i-765");
    }
}

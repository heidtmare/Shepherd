//! Rule catalog and configuration (`config/rules.toml`).
//!
//! A rule is a pure function of one case and the parties of other cases that share an identifier
//! with it; it returns detections, each identified within the case by a key. The engine turns
//! detections into findings: raised when new, updated when the evidence changes, resolved when no
//! longer detected.
//!
//! Adding a rule: implement [`Rule`] in a module here, add its [`RuleDef`] to [`CATALOG`], and
//! configure it in `config/rules.toml`. Bump `RuleDef::version` whenever its logic changes so
//! every case is re-evaluated at the next start.

pub mod a_number_name;
pub mod a_number_shared;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
        }
    }
}

/// The case under evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseRef {
    pub case_id: Uuid,
    pub receipt_number: String,
    pub form_type: String,
    pub status: String,
}

/// A party (applicant or beneficiary) of a case, with the case it belongs to. Carries no case
/// status: a status change does not re-evaluate the cases sharing an identifier with it.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Party {
    pub case_id: Uuid,
    pub receipt_number: String,
    pub form_type: String,
    /// `applicant` or `beneficiary`.
    pub role: String,
    pub family_name: Option<String>,
    pub given_name: Option<String>,
    pub a_number: Option<String>,
    pub date_of_birth: Option<String>,
}

pub struct Context<'a> {
    pub case: &'a CaseRef,
    /// Parties of `case`.
    pub parties: &'a [Party],
    /// Parties of other cases sharing an A-Number with a party of `case`, except A-Numbers in
    /// `over_limit`.
    pub related: &'a [Party],
    /// A-Numbers of `case` held by more than `case_limit` cases. Parties of other cases holding
    /// them are not loaded, and changes to them do not re-evaluate the other holders.
    pub over_limit: &'a [String],
    pub case_limit: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    /// Identity of the finding within the case; stable while the same condition holds.
    pub key: String,
    pub summary: String,
    pub evidence: Value,
    /// Other cases involved, sorted and without duplicates.
    pub related_cases: Vec<Uuid>,
}

pub trait Rule: Send + Sync {
    fn evaluate(&self, cx: &Context) -> Vec<Detection>;
}

pub struct RuleDef {
    pub id: &'static str,
    pub description: &'static str,
    /// Bumped when the rule's logic changes; part of the rules fingerprint.
    pub version: u32,
    /// Builds the rule from its configured parameters.
    pub build: fn(toml::Table) -> anyhow::Result<Box<dyn Rule>>,
}

pub const CATALOG: &[RuleDef] = &[a_number_name::DEF, a_number_shared::DEF];

/// Deserializes rule parameters, rejecting unknown keys when `P` denies them.
pub fn params<P: serde::de::DeserializeOwned>(t: toml::Table) -> anyhow::Result<P> {
    Ok(toml::Value::Table(t).try_into()?)
}

#[derive(Deserialize)]
struct Entry {
    #[serde(default = "yes")]
    enabled: bool,
    severity: Severity,
    #[serde(flatten)]
    params: toml::Table,
}

fn yes() -> bool {
    true
}

pub struct Configured {
    pub def: &'static RuleDef,
    pub enabled: bool,
    pub severity: Severity,
    pub params: toml::Table,
    rule: Box<dyn Rule>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RuleInfo {
    pub id: &'static str,
    pub description: &'static str,
    pub version: u32,
    pub enabled: bool,
    pub severity: Severity,
    #[schema(value_type = Object)]
    pub params: Value,
}

pub struct RuleSet {
    rules: Vec<Configured>,
    /// Changes when configuration or a rule version changes.
    pub fingerprint: String,
}

impl RuleSet {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("rules config {path}: {e}"))?;
        Self::parse(&src)
    }

    pub fn parse(src: &str) -> anyhow::Result<Self> {
        let entries: BTreeMap<String, Entry> = toml::from_str(src)?;
        let mut rules = Vec::new();
        for (id, e) in entries {
            let def = CATALOG
                .iter()
                .find(|d| d.id == id)
                .ok_or_else(|| anyhow::anyhow!("unknown rule {id:?}"))?;
            let rule =
                (def.build)(e.params.clone()).map_err(|err| anyhow::anyhow!("rule {id}: {err}"))?;
            rules.push(Configured {
                def,
                enabled: e.enabled,
                severity: e.severity,
                params: e.params,
                rule,
            });
        }
        let mut h = Sha256::new();
        for r in rules.iter().filter(|r| r.enabled) {
            h.update(format!(
                "{}@{}:{}:{}\n",
                r.def.id,
                r.def.version,
                r.severity.as_str(),
                serde_json::to_string(&r.params)?
            ));
        }
        Ok(Self {
            rules,
            fingerprint: hex::encode(h.finalize()),
        })
    }

    pub fn info(&self) -> Vec<RuleInfo> {
        self.rules
            .iter()
            .map(|r| RuleInfo {
                id: r.def.id,
                description: r.def.description,
                version: r.def.version,
                enabled: r.enabled,
                severity: r.severity,
                params: serde_json::to_value(&r.params).unwrap_or(Value::Null),
            })
            .collect()
    }

    pub fn configured(&self, id: &str) -> Option<&Configured> {
        self.rules.iter().find(|r| r.def.id == id)
    }

    /// Detections of every enabled rule.
    pub fn evaluate(&self, cx: &Context) -> Vec<(&Configured, Detection)> {
        self.rules
            .iter()
            .filter(|r| r.enabled)
            .flat_map(|r| r.rule.evaluate(cx).into_iter().map(move |d| (r, d)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_config_is_valid() {
        let set = RuleSet::parse(include_str!("../../config/rules.toml")).unwrap();
        assert!(
            set.info()
                .iter()
                .any(|r| r.id == a_number_name::DEF.id && r.enabled)
        );
    }

    #[test]
    fn rejects_unknown_rules_and_params() {
        assert!(RuleSet::parse("[no_such_rule]\nseverity = \"low\"").is_err());
        assert!(RuleSet::parse("[a_number_name_mismatch]\nseverity = \"low\"\nbogus = 1").is_err());
        assert!(RuleSet::parse("[a_number_name_mismatch]\nseverity = \"urgent\"").is_err());
    }

    #[test]
    fn fingerprint_tracks_enabled_configuration() {
        let a = RuleSet::parse("[a_number_name_mismatch]\nseverity = \"high\"").unwrap();
        let b = RuleSet::parse("[a_number_name_mismatch]\nseverity = \"low\"").unwrap();
        let off = RuleSet::parse("[a_number_name_mismatch]\nseverity = \"high\"\nenabled = false")
            .unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
        assert_ne!(a.fingerprint, off.fingerprint);
        assert_eq!(off.fingerprint, RuleSet::parse("").unwrap().fingerprint);
    }
}

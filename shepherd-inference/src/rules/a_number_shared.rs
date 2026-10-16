//! `a_number_widely_shared`: an A-Number of the case recorded on more cases than
//! `A_NUMBER_CASE_LIMIT`.
//!
//! One person rarely has that many cases; the number is more likely a placeholder, a recurring
//! misread or a shared fabricated identity. Names are not compared for such A-Numbers, and the
//! evidence names only the case's own holders, so it does not change as other cases come and go.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::json;

use super::{Context, Detection, Party, Rule, RuleDef, a_number_name::party_json, params};

pub const DEF: RuleDef = RuleDef {
    id: "a_number_widely_shared",
    description: "An A-Number is recorded on more cases than the configured limit.",
    version: 1,
    build,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {}

fn build(t: toml::Table) -> anyhow::Result<Box<dyn Rule>> {
    let Params {} = params(t)?;
    Ok(Box::new(ANumberShared))
}

pub struct ANumberShared;

impl Rule for ANumberShared {
    fn evaluate(&self, cx: &Context) -> Vec<Detection> {
        let mut holders: BTreeMap<&str, Vec<&Party>> = BTreeMap::new();
        for p in cx.parties {
            if let Some(a) = p.a_number.as_deref()
                && cx.over_limit.iter().any(|o| o == a)
            {
                holders.entry(a).or_default().push(p);
            }
        }
        holders
            .into_iter()
            .map(|(a_number, own)| Detection {
                key: a_number.to_owned(),
                summary: format!(
                    "A-Number A{a_number} is recorded on more than {} cases; names were not compared",
                    cx.case_limit
                ),
                evidence: json!({
                    "a_number": a_number,
                    "case_limit": cx.case_limit,
                    "holders": own.iter().map(|p| party_json(p)).collect::<Vec<_>>(),
                }),
                related_cases: vec![],
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::rules::CaseRef;

    #[test]
    fn flags_a_numbers_over_limit() {
        let me = Uuid::now_v7();
        let party = |role: &str, a: &str| Party {
            case_id: me,
            receipt_number: "SHP2600000001".into(),
            form_type: "uscis.i-130".into(),
            role: role.into(),
            family_name: Some("Smith".into()),
            given_name: None,
            a_number: Some(a.into()),
            date_of_birth: None,
        };
        let parties = [
            party("applicant", "000000000"),
            party("beneficiary", "087654321"),
        ];
        let d = ANumberShared.evaluate(&Context {
            case: &CaseRef {
                case_id: me,
                receipt_number: "SHP2600000001".into(),
                form_type: "uscis.i-130".into(),
                status: "open".into(),
            },
            parties: &parties,
            related: &[],
            over_limit: &["000000000".into()],
            case_limit: 50,
        });
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].key, "000000000");
        assert!(d[0].related_cases.is_empty());
        assert_eq!(d[0].evidence["holders"][0]["role"], "applicant");
    }

    #[test]
    fn takes_no_params() {
        assert!(build(toml::Table::new()).is_ok());
        let mut t = toml::Table::new();
        t.insert("limit".into(), toml::Value::Integer(5));
        assert!(build(t).is_err());
    }
}

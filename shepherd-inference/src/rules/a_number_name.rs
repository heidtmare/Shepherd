//! `a_number_name_mismatch`: an A-Number recorded for people whose names do not match.
//!
//! An A-Number identifies one person. The same number under a different name, on another case or
//! for the other party of the same case, indicates a borrowed or fabricated identity, or a
//! misread number. One finding per A-Number of the case. A-Numbers over the case limit are not
//! compared (`a_number_widely_shared` reports them).

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::{Context, Detection, Party, Rule, RuleDef, params};
use crate::names::{self, NameMatch};

pub const DEF: RuleDef = RuleDef {
    id: "a_number_name_mismatch",
    description: "The same A-Number is recorded for people whose names do not match.",
    version: 2,
    build,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    #[serde(default = "default_edit_ratio")]
    edit_ratio: f64,
}

fn default_edit_ratio() -> f64 {
    0.25
}

fn build(t: toml::Table) -> anyhow::Result<Box<dyn Rule>> {
    let p: Params = params(t)?;
    anyhow::ensure!(
        (0.0..=0.5).contains(&p.edit_ratio),
        "edit_ratio must be between 0 and 0.5"
    );
    Ok(Box::new(ANumberName {
        edit_ratio: p.edit_ratio,
    }))
}

pub struct ANumberName {
    edit_ratio: f64,
}

fn name_tokens(p: &Party) -> Vec<String> {
    names::tokens(p.family_name.as_deref(), p.given_name.as_deref())
}

fn display_name(p: &Party) -> String {
    match (p.family_name.as_deref(), p.given_name.as_deref()) {
        (Some(f), Some(g)) => format!("{f}, {g}"),
        (Some(n), None) | (None, Some(n)) => n.to_owned(),
        (None, None) => "(no name)".to_owned(),
    }
}

pub(super) fn party_json(p: &Party) -> serde_json::Value {
    json!({
        "case_id": p.case_id, "receipt_number": p.receipt_number, "form_type": p.form_type,
        "role": p.role, "family_name": p.family_name, "given_name": p.given_name,
        "date_of_birth": p.date_of_birth,
    })
}

impl Rule for ANumberName {
    fn evaluate(&self, cx: &Context) -> Vec<Detection> {
        let mut holders: BTreeMap<&str, Vec<&Party>> = BTreeMap::new();
        for p in cx.parties {
            if let Some(a) = p.a_number.as_deref().filter(|a| !a.is_empty())
                && !cx.over_limit.iter().any(|o| o == a)
                && !name_tokens(p).is_empty()
            {
                holders.entry(a).or_default().push(p);
            }
        }
        let mut out = Vec::new();
        for (a_number, own) in holders {
            let own_tokens: Vec<_> = own.iter().map(|p| name_tokens(p)).collect();
            let others = cx
                .parties
                .iter()
                .chain(cx.related)
                .filter(|o| o.a_number.as_deref() == Some(a_number));
            let (mut conflicting, mut consistent) = (Vec::new(), Vec::new());
            for o in others {
                let t = name_tokens(o);
                if t.is_empty() {
                    continue;
                }
                let against: Vec<&str> = own
                    .iter()
                    .zip(&own_tokens)
                    .filter(|(p, pt)| {
                        !std::ptr::eq(**p, o)
                            && names::compare(pt, &t, self.edit_ratio) == NameMatch::Different
                    })
                    .map(|(p, _)| p.role.as_str())
                    .collect();
                if !against.is_empty() {
                    conflicting.push((o, against));
                } else if o.case_id != cx.case.case_id {
                    consistent.push(o);
                }
            }
            if conflicting.is_empty() {
                continue;
            }
            let external: Vec<&Party> = conflicting
                .iter()
                .map(|(o, _)| *o)
                .filter(|o| o.case_id != cx.case.case_id)
                .collect();
            let named = |p: &Party| format!("the {} {}", p.role, display_name(p));
            let mut summary = if external.len() < conflicting.len() {
                format!(
                    "A-Number A{a_number} is recorded for both {}",
                    own.iter()
                        .map(|p| named(p))
                        .collect::<Vec<_>>()
                        .join(" and ")
                )
            } else {
                format!(
                    "A-Number A{a_number} of {}",
                    own.iter()
                        .map(|p| named(p))
                        .collect::<Vec<_>>()
                        .join(" and ")
                )
            };
            if let Some(first) = external.first() {
                summary.push_str(&format!(
                    "{} {} ({} {})",
                    if external.len() < conflicting.len() {
                        "; also recorded for"
                    } else {
                        " is also recorded for"
                    },
                    display_name(first),
                    first.receipt_number,
                    first.role
                ));
                if external.len() > 1 {
                    summary.push_str(&format!(" and {} other(s)", external.len() - 1));
                }
            }
            let related_cases: BTreeSet<Uuid> = external.iter().map(|o| o.case_id).collect();
            out.push(Detection {
                key: a_number.to_owned(),
                summary,
                evidence: json!({
                    "a_number": a_number,
                    "holders": own.iter().map(|p| party_json(p)).collect::<Vec<_>>(),
                    "conflicting": conflicting.iter().map(|(o, against)| {
                        let mut v = party_json(o);
                        v["conflicts_with"] = json!(against);
                        v
                    }).collect::<Vec<_>>(),
                    "consistent": consistent.iter().map(|o| party_json(o)).collect::<Vec<_>>(),
                }),
                related_cases: related_cases.into_iter().collect(),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::CaseRef;

    fn party(case: Uuid, receipt: &str, role: &str, family: &str, given: &str, a: &str) -> Party {
        Party {
            case_id: case,
            receipt_number: receipt.into(),
            form_type: "uscis.i-765".into(),
            role: role.into(),
            family_name: Some(family.into()).filter(|s: &String| !s.is_empty()),
            given_name: Some(given.into()).filter(|s: &String| !s.is_empty()),
            a_number: Some(a.into()).filter(|s: &String| !s.is_empty()),
            date_of_birth: None,
        }
    }

    fn case(id: Uuid) -> CaseRef {
        CaseRef {
            case_id: id,
            receipt_number: "SHP2600000001".into(),
            form_type: "uscis.i-765".into(),
            status: "open".into(),
        }
    }

    fn rule() -> ANumberName {
        ANumberName { edit_ratio: 0.25 }
    }

    fn run(c: &CaseRef, parties: &[Party], related: &[Party]) -> Vec<Detection> {
        rule().evaluate(&Context {
            case: c,
            parties,
            related,
            over_limit: &[],
            case_limit: 50,
        })
    }

    #[test]
    fn flags_other_case_with_different_name() {
        let (me, other, third) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let c = case(me);
        let mine = [party(
            me,
            "SHP2600000001",
            "applicant",
            "Nguyen",
            "Linh",
            "012345678",
        )];
        let related = [
            party(
                other,
                "SHP2600000002",
                "applicant",
                "Okafor",
                "Chinedu",
                "012345678",
            ),
            party(
                third,
                "SHP2600000003",
                "applicant",
                "NGUYEN",
                "Lihn",
                "012345678",
            ),
        ];
        let d = run(&c, &mine, &related);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].key, "012345678");
        assert_eq!(d[0].related_cases, [other]);
        assert_eq!(
            d[0].summary,
            "A-Number A012345678 of the applicant Nguyen, Linh is also recorded for Okafor, Chinedu (SHP2600000002 applicant)"
        );
        assert_eq!(
            d[0].evidence["conflicting"][0]["conflicts_with"],
            json!(["applicant"])
        );
        assert_eq!(d[0].evidence["consistent"][0]["case_id"], json!(third));
    }

    #[test]
    fn name_variants_and_missing_values_do_not_fire() {
        let (me, other) = (Uuid::now_v7(), Uuid::now_v7());
        let c = case(me);
        let mine = [party(
            me,
            "SHP2600000001",
            "applicant",
            "Garcia Lopez",
            "María",
            "012345678",
        )];
        let related = [
            party(
                other,
                "SHP2600000002",
                "applicant",
                "Garcia",
                "Maria",
                "012345678",
            ),
            party(other, "SHP2600000002", "beneficiary", "", "", "012345678"),
            party(other, "SHP2600000002", "applicant", "Okafor", "Chinedu", ""),
        ];
        assert!(run(&c, &mine, &related).is_empty());
        let unnamed = [party(me, "SHP2600000001", "applicant", "", "", "012345678")];
        let conflicting = [party(
            other,
            "SHP2600000002",
            "applicant",
            "Okafor",
            "Chinedu",
            "012345678",
        )];
        assert!(run(&c, &unnamed, &conflicting).is_empty());
    }

    #[test]
    fn flags_petitioner_and_beneficiary_sharing_a_number() {
        let me = Uuid::now_v7();
        let c = case(me);
        let mine = [
            party(
                me,
                "SHP2600000001",
                "applicant",
                "Smith",
                "John",
                "087654321",
            ),
            party(
                me,
                "SHP2600000001",
                "beneficiary",
                "Mensah",
                "Ama",
                "087654321",
            ),
        ];
        let d = run(&c, &mine, &[]);
        assert_eq!(d.len(), 1);
        assert!(d[0].related_cases.is_empty());
        assert_eq!(d[0].evidence["holders"].as_array().unwrap().len(), 2);
        assert_eq!(d[0].evidence["conflicting"].as_array().unwrap().len(), 2);
        assert_eq!(
            d[0].summary,
            "A-Number A087654321 is recorded for both the applicant Smith, John and the beneficiary Mensah, Ama"
        );
    }

    #[test]
    fn same_person_as_both_parties_does_not_fire() {
        let me = Uuid::now_v7();
        let mine = [
            party(
                me,
                "SHP2600000001",
                "applicant",
                "Smith",
                "John",
                "087654321",
            ),
            party(
                me,
                "SHP2600000001",
                "beneficiary",
                "SMITH",
                "JOHN",
                "087654321",
            ),
        ];
        assert!(run(&case(me), &mine, &[]).is_empty());
    }

    #[test]
    fn a_numbers_over_limit_are_not_compared() {
        let me = Uuid::now_v7();
        let mine = [
            party(
                me,
                "SHP2600000001",
                "applicant",
                "Smith",
                "John",
                "087654321",
            ),
            party(
                me,
                "SHP2600000001",
                "beneficiary",
                "Mensah",
                "Ama",
                "087654321",
            ),
        ];
        let d = rule().evaluate(&Context {
            case: &case(me),
            parties: &mine,
            related: &[],
            over_limit: &["087654321".into()],
            case_limit: 50,
        });
        assert!(d.is_empty());
    }

    #[test]
    fn validates_params() {
        assert!(build(toml::Table::new()).is_ok());
        let mut t = toml::Table::new();
        t.insert("edit_ratio".into(), toml::Value::Float(0.9));
        assert!(build(t).is_err());
    }
}

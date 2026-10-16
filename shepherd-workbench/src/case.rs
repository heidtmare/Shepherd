//! Case lifecycle rules: receipt numbers, status transitions, criteria and decision validation.
//! Pure functions; persistence is in `case_db`, HTTP in `case_api`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;

use crate::{
    benefits::{Benefit, Benefits},
    receipt::Rejection,
};

pub const CASE_SCHEMA: &str = "workbench.case.v1";
pub const DECISION_SCHEMA: &str = "workbench.decision.v1";
pub const NOTICE_SCHEMA: &str = "workbench.notice.v1";

/// Minimum rationale length for denials, withdrawals and notice rescissions.
pub const MIN_RATIONALE: usize = 20;
const MAX_TEXT: usize = 10_000;
const MAX_NOTE: usize = 2_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Unassigned, awaiting an officer.
    Open,
    /// Assigned to an officer.
    InReview,
    /// RFE or NOID outstanding.
    AwaitingResponse,
    Decided,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::InReview => "in_review",
            Status::AwaitingResponse => "awaiting_response",
            Status::Decided => "decided",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "open" => Status::Open,
            "in_review" => Status::InReview,
            "awaiting_response" => Status::AwaitingResponse,
            "decided" => Status::Decided,
            _ => return None,
        })
    }

    /// Status after assignment changes or a notice response, for an undecided case.
    pub fn working(assignee: Option<&str>) -> Self {
        if assignee.is_some() {
            Status::InReview
        } else {
            Status::Open
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Approved,
    Denied,
    /// Withdrawn by the applicant or petitioner.
    Withdrawn,
    /// Administratively closed without adjudication; see [`ClosureReason`].
    Closed,
}

impl Outcome {
    pub const ALL: [Outcome; 4] = [
        Outcome::Approved,
        Outcome::Denied,
        Outcome::Withdrawn,
        Outcome::Closed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Approved => "approved",
            Outcome::Denied => "denied",
            Outcome::Withdrawn => "withdrawn",
            Outcome::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClosureReason {
    /// Duplicate of another filing.
    Duplicate,
    /// Opened for a document that is not a benefit request, or opened twice.
    OpenedInError,
    /// Jurisdiction transferred to another office or agency.
    Transferred,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    /// Request for evidence.
    Rfe,
    /// Notice of intent to deny.
    Noid,
}

impl NoticeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NoticeKind::Rfe => "rfe",
            NoticeKind::Noid => "noid",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CriterionResult {
    Met,
    NotMet,
    /// Not applicable; only allowed for criteria with `required = false`.
    Na,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct Assessment {
    pub result: CriterionResult,
    pub note: Option<String>,
    pub by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AssessmentInput {
    pub result: CriterionResult,
    pub note: Option<String>,
}

fn reject(message: impl Into<String>, fields: BTreeMap<String, String>) -> Rejection {
    Rejection {
        message: message.into(),
        fields,
    }
}

fn one(field: &str, problem: impl Into<String>) -> Rejection {
    let problem = problem.into();
    reject(
        problem.clone(),
        BTreeMap::from([(field.to_owned(), problem)]),
    )
}

/// `AAA` + 10 digits, e.g. `IOE2612345678`. Accepts lower case, spaces and dashes.
pub fn normalize_receipt_number(s: &str) -> Option<String> {
    let n: String = s
        .chars()
        .filter(|c| !matches!(c, ' ' | '-'))
        .collect::<String>()
        .to_ascii_uppercase();
    let (prefix, digits) = n.split_at_checked(3)?;
    (prefix.chars().all(|c| c.is_ascii_uppercase())
        && digits.len() == 10
        && digits.chars().all(|c| c.is_ascii_digit()))
    .then_some(n)
}

/// Prefix + 2-digit year + 8-digit sequence.
pub fn receipt_number(prefix: &str, year: i32, seq: i64) -> String {
    format!(
        "{prefix}{:02}{:08}",
        year.rem_euclid(100),
        seq.rem_euclid(100_000_000)
    )
}

pub fn valid_receipt_prefix(p: &str) -> bool {
    p.len() == 3 && p.chars().all(|c| c.is_ascii_uppercase())
}

/// Merges officer input into the stored assessments. `None` clears a criterion.
pub fn apply_assessments(
    benefit: &Benefit,
    current: &BTreeMap<String, Assessment>,
    updates: &BTreeMap<String, Option<AssessmentInput>>,
    actor: &str,
    now: OffsetDateTime,
) -> Result<BTreeMap<String, Assessment>, Rejection> {
    let mut out = current.clone();
    let mut problems = BTreeMap::new();
    for (id, u) in updates {
        let Some(c) = benefit.criterion(id) else {
            problems.insert(id.clone(), "not a criterion of this benefit".into());
            continue;
        };
        match u {
            None => {
                out.remove(id);
            }
            Some(u) => {
                if u.result == CriterionResult::Na && c.required {
                    problems.insert(id.clone(), "required criterion cannot be n/a".into());
                    continue;
                }
                let note = u
                    .note
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_owned);
                if note.as_ref().is_some_and(|n| n.chars().count() > MAX_NOTE) {
                    problems.insert(
                        id.clone(),
                        format!("note longer than {MAX_NOTE} characters"),
                    );
                    continue;
                }
                if u.result == CriterionResult::NotMet && note.is_none() {
                    problems.insert(id.clone(), "explain why the criterion is not met".into());
                    continue;
                }
                let unchanged = out
                    .get(id)
                    .is_some_and(|a| a.result == u.result && a.note == note);
                if !unchanged {
                    out.insert(
                        id.clone(),
                        Assessment {
                            result: u.result,
                            note,
                            by: actor.to_owned(),
                            at: now,
                        },
                    );
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(out)
    } else {
        Err(reject(
            format!("{} criterion update(s) rejected", problems.len()),
            problems,
        ))
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct DecisionInput {
    pub outcome: Outcome,
    /// Denial ground ids from `benefits.toml`.
    #[serde(default)]
    pub grounds: Vec<String>,
    #[serde(default)]
    pub rationale: String,
    /// Required for `closed`, otherwise absent.
    pub closure_reason: Option<ClosureReason>,
}

/// Case state a decision is checked against.
pub struct DecisionContext<'a> {
    pub benefits: &'a Benefits,
    pub benefit: &'a Benefit,
    pub status: Status,
    pub criteria: &'a BTreeMap<String, Assessment>,
    /// Receipting tasks still open on the case's documents.
    pub open_tasks: i64,
    /// The primary document has final form values (a receipt, or a recognition needing none).
    pub has_form_values: bool,
    /// A notice is outstanding and past its due date.
    pub response_overdue: bool,
    /// Open inference findings not dismissed.
    pub open_findings: usize,
}

/// Validates a decision, or lists what must be resolved first.
pub fn check_decision(ctx: &DecisionContext<'_>, d: &DecisionInput) -> Result<(), Rejection> {
    let mut p = BTreeMap::new();
    let rationale = d.rationale.trim();
    if rationale.chars().count() > MAX_TEXT {
        p.insert(
            "rationale".into(),
            format!("longer than {MAX_TEXT} characters"),
        );
    }
    for g in &d.grounds {
        if ctx.benefits.ground(g).is_none() {
            p.insert(format!("grounds.{g}"), "unknown ground".into());
        }
    }
    let abandonment = d.grounds.iter().any(|g| g == "abandonment");
    match ctx.status {
        // Unassigned cases are closed by managers; the API checks who may act.
        Status::Open | Status::AwaitingResponse if d.outcome == Outcome::Closed => {}
        Status::InReview => {}
        Status::AwaitingResponse
            if d.outcome == Outcome::Withdrawn
                || (d.outcome == Outcome::Denied && abandonment && ctx.response_overdue) => {}
        Status::AwaitingResponse => {
            p.insert(
                "status".into(),
                "a notice is outstanding: record the response, or deny for abandonment after the due date"
                    .into(),
            );
        }
        s => {
            p.insert("status".into(), format!("case is {}", s.as_str()));
        }
    }
    let needs_record = !(matches!(d.outcome, Outcome::Withdrawn | Outcome::Closed) || abandonment);
    if d.closure_reason.is_some() != (d.outcome == Outcome::Closed) {
        p.insert(
            "closure_reason".into(),
            if d.outcome == Outcome::Closed {
                "give the closure reason"
            } else {
                "only for administrative closure"
            }
            .into(),
        );
    }
    if needs_record {
        if ctx.open_tasks > 0 {
            p.insert(
                "receipting".into(),
                format!("{} receipting task(s) still open", ctx.open_tasks),
            );
        }
        if !ctx.has_form_values {
            p.insert(
                "receipting".into(),
                "primary form has not been receipted".into(),
            );
        }
    }
    match d.outcome {
        Outcome::Approved => {
            if !d.grounds.is_empty() {
                p.insert("grounds".into(), "approvals cite no denial grounds".into());
            }
            if ctx.open_findings > 0 {
                p.insert(
                    "findings".into(),
                    format!(
                        "{} open finding(s): correct the case data or dismiss",
                        ctx.open_findings
                    ),
                );
            }
            for c in &ctx.benefit.criteria {
                let key = format!("criteria.{}", c.id);
                match ctx.criteria.get(&c.id).map(|a| a.result) {
                    None => {
                        p.insert(key, "not assessed".into());
                    }
                    Some(CriterionResult::NotMet) => {
                        p.insert(key, "not met".into());
                    }
                    Some(CriterionResult::Na) if c.required => {
                        p.insert(key, "required criterion marked n/a".into());
                    }
                    _ => {}
                }
            }
        }
        Outcome::Denied => {
            if d.grounds.is_empty() {
                p.insert("grounds".into(), "cite at least one ground".into());
            }
            if abandonment && !ctx.response_overdue {
                p.insert(
                    "grounds.abandonment".into(),
                    "no notice is past its due date".into(),
                );
            }
            if rationale.chars().count() < MIN_RATIONALE {
                p.insert(
                    "rationale".into(),
                    format!("at least {MIN_RATIONALE} characters"),
                );
            }
        }
        Outcome::Withdrawn | Outcome::Closed => {
            if !d.grounds.is_empty() {
                p.insert(
                    "grounds".into(),
                    if d.outcome == Outcome::Closed {
                        "closures cite no denial grounds"
                    } else {
                        "withdrawals cite no denial grounds"
                    }
                    .into(),
                );
            }
            if rationale.chars().count() < MIN_RATIONALE {
                p.insert(
                    "rationale".into(),
                    format!("at least {MIN_RATIONALE} characters"),
                );
            }
        }
    }
    if p.is_empty() {
        Ok(())
    } else {
        Err(reject(
            format!("cannot record {}: {} issue(s)", d.outcome.as_str(), p.len()),
            p,
        ))
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct NoticeInput {
    pub kind: NoticeKind,
    /// Evidence requested or deficiencies found; one entry per item.
    pub items: Vec<String>,
    pub message: Option<String>,
    /// `YYYY-MM-DD`; defaults to the configured response window.
    pub due_date: Option<String>,
}

/// Validated notice: trimmed items and due date.
pub fn check_notice(
    n: &NoticeInput,
    status: Status,
    today: time::Date,
    default_days: i64,
) -> Result<(Vec<String>, time::Date), Rejection> {
    if status != Status::InReview {
        return Err(one(
            "status",
            format!("notices are issued in review; case is {}", status.as_str()),
        ));
    }
    let items: Vec<String> = n
        .items
        .iter()
        .map(|i| i.trim().to_owned())
        .filter(|i| !i.is_empty())
        .collect();
    if items.is_empty() {
        return Err(one("items", "list at least one item"));
    }
    if items.len() > 50 || items.iter().any(|i| i.chars().count() > MAX_NOTE) {
        return Err(one(
            "items",
            format!("at most 50 items of {MAX_NOTE} characters"),
        ));
    }
    let due = match &n.due_date {
        Some(s) => parse_date(s).map_err(|e| one("due_date", e))?,
        None => today + time::Duration::days(default_days),
    };
    if due <= today {
        return Err(one("due_date", "must be after today"));
    }
    Ok((items, due))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ExtensionInput {
    /// `YYYY-MM-DD`; after the current due date and today.
    pub due_date: String,
    pub reason: String,
}

/// Validated extension: new due date and trimmed reason.
pub fn check_extension(
    e: &ExtensionInput,
    current_due: time::Date,
    today: time::Date,
) -> Result<(time::Date, String), Rejection> {
    let mut p = BTreeMap::new();
    let reason = e.reason.trim();
    if reason.is_empty() || reason.chars().count() > MAX_NOTE {
        p.insert("reason".into(), format!("1 to {MAX_NOTE} characters"));
    }
    let due = match parse_date(&e.due_date) {
        Ok(d) if d <= current_due => {
            p.insert(
                "due_date".into(),
                format!(
                    "must be after the current due date {}",
                    format_date(current_due)
                ),
            );
            None
        }
        Ok(d) if d <= today => {
            p.insert("due_date".into(), "must be after today".into());
            None
        }
        Ok(d) => Some(d),
        Err(err) => {
            p.insert("due_date".into(), err);
            None
        }
    };
    match due {
        Some(d) if p.is_empty() => Ok((d, reason.to_owned())),
        _ => Err(reject(format!("cannot extend: {} issue(s)", p.len()), p)),
    }
}

/// Trimmed rescission reason.
pub fn check_rescission(reason: &str) -> Result<String, Rejection> {
    let r = reason.trim();
    let n = r.chars().count();
    if !(MIN_RATIONALE..=MAX_NOTE).contains(&n) {
        return Err(one(
            "reason",
            format!("{MIN_RATIONALE} to {MAX_NOTE} characters"),
        ));
    }
    Ok(r.to_owned())
}

const DATE: &[time::format_description::BorrowedFormatItem<'static>] =
    time::macros::format_description!("[year]-[month]-[day]");

pub fn parse_date(s: &str) -> Result<time::Date, String> {
    time::Date::parse(s.trim(), DATE).map_err(|_| "expected YYYY-MM-DD".to_owned())
}

pub fn format_date(d: time::Date) -> String {
    d.format(DATE).expect("date formats")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::benefits::{Criterion, Ground};

    fn benefits() -> Benefits {
        Benefits {
            e_notification: None,
            grounds: ["ineligible", "abandonment"]
                .iter()
                .map(|g| Ground {
                    id: g.to_string(),
                    label: g.to_string(),
                })
                .collect(),
            benefits: vec![Benefit {
                form_type: "t".into(),
                name: "T".into(),
                facts: Default::default(),
                highlights: vec![],
                criteria: vec![
                    Criterion {
                        id: "identity".into(),
                        label: "Identity".into(),
                        required: true,
                    },
                    Criterion {
                        id: "prior".into(),
                        label: "Prior".into(),
                        required: false,
                    },
                ],
            }],
        }
    }

    fn assessed(pairs: &[(&str, CriterionResult)]) -> BTreeMap<String, Assessment> {
        pairs
            .iter()
            .map(|(id, r)| {
                (
                    id.to_string(),
                    Assessment {
                        result: *r,
                        note: None,
                        by: "ana".into(),
                        at: OffsetDateTime::UNIX_EPOCH,
                    },
                )
            })
            .collect()
    }

    fn ctx<'a>(
        b: &'a Benefits,
        status: Status,
        criteria: &'a BTreeMap<String, Assessment>,
    ) -> DecisionContext<'a> {
        DecisionContext {
            benefits: b,
            benefit: &b.benefits[0],
            status,
            criteria,
            open_tasks: 0,
            has_form_values: true,
            response_overdue: false,
            open_findings: 0,
        }
    }

    fn decision(outcome: Outcome, grounds: &[&str], rationale: &str) -> DecisionInput {
        DecisionInput {
            outcome,
            grounds: grounds.iter().map(|g| g.to_string()).collect(),
            rationale: rationale.into(),
            closure_reason: None,
        }
    }

    #[test]
    fn receipt_numbers() {
        assert_eq!(
            normalize_receipt_number("ioe-261-234-5678").as_deref(),
            Some("IOE2612345678")
        );
        assert_eq!(normalize_receipt_number("IO12612345678"), None);
        assert_eq!(normalize_receipt_number("IOE261234567"), None);
        assert_eq!(normalize_receipt_number("ÉOE2612345678"), None);
        assert_eq!(receipt_number("SHP", 2026, 42), "SHP2600000042");
        assert!(normalize_receipt_number(&receipt_number("SHP", 2026, 123_456_789)).is_some());
        assert!(valid_receipt_prefix("IOE") && !valid_receipt_prefix("io1"));
    }

    #[test]
    fn approval_requires_every_criterion() {
        let b = benefits();
        let partial = assessed(&[("identity", CriterionResult::Met)]);
        let err = check_decision(
            &ctx(&b, Status::InReview, &partial),
            &decision(Outcome::Approved, &[], ""),
        )
        .unwrap_err();
        assert_eq!(err.fields.keys().collect::<Vec<_>>(), ["criteria.prior"]);

        let all = assessed(&[
            ("identity", CriterionResult::Met),
            ("prior", CriterionResult::Na),
        ]);
        let c = ctx(&b, Status::InReview, &all);
        assert!(check_decision(&c, &decision(Outcome::Approved, &[], "")).is_ok());

        let mut flagged = ctx(&b, Status::InReview, &all);
        flagged.open_findings = 1;
        let err = check_decision(&flagged, &decision(Outcome::Approved, &[], "")).unwrap_err();
        assert_eq!(err.fields.keys().collect::<Vec<_>>(), ["findings"]);
        assert!(
            check_decision(
                &flagged,
                &decision(
                    Outcome::Denied,
                    &["ineligible"],
                    "long enough rationale text"
                )
            )
            .is_ok()
        );

        let mut pending = ctx(&b, Status::InReview, &all);
        pending.open_tasks = 1;
        pending.has_form_values = false;
        let err = check_decision(&pending, &decision(Outcome::Approved, &[], "")).unwrap_err();
        assert!(err.fields.contains_key("receipting"));

        let not_met = assessed(&[
            ("identity", CriterionResult::NotMet),
            ("prior", CriterionResult::Met),
        ]);
        assert!(
            check_decision(
                &ctx(&b, Status::InReview, &not_met),
                &decision(Outcome::Approved, &[], "")
            )
            .is_err()
        );
    }

    #[test]
    fn denial_requires_grounds_and_rationale() {
        let b = benefits();
        let none = BTreeMap::new();
        let c = ctx(&b, Status::InReview, &none);
        let err = check_decision(&c, &decision(Outcome::Denied, &[], "short")).unwrap_err();
        assert!(err.fields.contains_key("grounds") && err.fields.contains_key("rationale"));
        let err = check_decision(
            &c,
            &decision(Outcome::Denied, &["bogus"], "long enough rationale text"),
        )
        .unwrap_err();
        assert!(err.fields.contains_key("grounds.bogus"));
        assert!(
            check_decision(
                &c,
                &decision(
                    Outcome::Denied,
                    &["ineligible"],
                    "long enough rationale text"
                )
            )
            .is_ok()
        );
    }

    #[test]
    fn outstanding_notice_limits_outcomes() {
        let b = benefits();
        let none = BTreeMap::new();
        let mut c = ctx(&b, Status::AwaitingResponse, &none);
        let deny = decision(
            Outcome::Denied,
            &["abandonment"],
            "no response to the request for evidence",
        );
        assert!(
            check_decision(&c, &deny)
                .unwrap_err()
                .fields
                .contains_key("status")
        );
        c.response_overdue = true;
        c.open_tasks = 2;
        c.has_form_values = false;
        assert!(check_decision(&c, &deny).is_ok());
        assert!(
            check_decision(
                &c,
                &decision(
                    Outcome::Denied,
                    &["ineligible"],
                    "long enough rationale text"
                )
            )
            .is_err()
        );
        assert!(
            check_decision(
                &c,
                &decision(Outcome::Withdrawn, &[], "applicant withdrew by letter")
            )
            .is_ok()
        );
        let decided = ctx(&b, Status::Decided, &none);
        assert!(
            check_decision(
                &decided,
                &decision(Outcome::Withdrawn, &[], "applicant withdrew by letter")
            )
            .is_err()
        );
    }

    #[test]
    fn administrative_closure() {
        let b = benefits();
        let none = BTreeMap::new();
        let close = DecisionInput {
            closure_reason: Some(ClosureReason::Duplicate),
            ..decision(Outcome::Closed, &[], "duplicate of IOE2600000041")
        };
        for s in [Status::Open, Status::InReview, Status::AwaitingResponse] {
            let mut c = ctx(&b, s, &none);
            c.open_tasks = 1;
            c.has_form_values = false;
            c.open_findings = 2;
            assert!(check_decision(&c, &close).is_ok(), "{s:?}");
        }
        assert!(check_decision(&ctx(&b, Status::Decided, &none), &close).is_err());
        let c = ctx(&b, Status::InReview, &none);
        let err =
            check_decision(&c, &decision(Outcome::Closed, &["ineligible"], "short")).unwrap_err();
        assert_eq!(
            err.fields.keys().collect::<Vec<_>>(),
            ["closure_reason", "grounds", "rationale"]
        );
        let withdraw = DecisionInput {
            closure_reason: Some(ClosureReason::Other),
            ..decision(Outcome::Withdrawn, &[], "applicant withdrew by letter")
        };
        assert!(
            check_decision(&c, &withdraw)
                .unwrap_err()
                .fields
                .contains_key("closure_reason")
        );
        // Only closure is possible on an unassigned case.
        assert!(
            check_decision(
                &ctx(&b, Status::Open, &none),
                &decision(Outcome::Withdrawn, &[], "applicant withdrew by letter")
            )
            .is_err()
        );
    }

    #[test]
    fn assessments_merge_and_validate() {
        let b = benefits();
        let now = OffsetDateTime::UNIX_EPOCH;
        let upd = BTreeMap::from([
            (
                "identity".to_string(),
                Some(AssessmentInput {
                    result: CriterionResult::Na,
                    note: None,
                }),
            ),
            (
                "nope".to_string(),
                Some(AssessmentInput {
                    result: CriterionResult::Met,
                    note: None,
                }),
            ),
            (
                "prior".to_string(),
                Some(AssessmentInput {
                    result: CriterionResult::NotMet,
                    note: Some("  ".into()),
                }),
            ),
        ]);
        let err =
            apply_assessments(&b.benefits[0], &BTreeMap::new(), &upd, "ana", now).unwrap_err();
        assert_eq!(err.fields.len(), 3);

        let upd = BTreeMap::from([(
            "identity".to_string(),
            Some(AssessmentInput {
                result: CriterionResult::Met,
                note: Some(" passport verified ".into()),
            }),
        )]);
        let out = apply_assessments(&b.benefits[0], &BTreeMap::new(), &upd, "ana", now).unwrap();
        assert_eq!(out["identity"].note.as_deref(), Some("passport verified"));
        let later = now + time::Duration::hours(1);
        let same = apply_assessments(&b.benefits[0], &out, &upd, "ben", later).unwrap();
        assert_eq!(same["identity"].by, "ana");
        let cleared = apply_assessments(
            &b.benefits[0],
            &out,
            &BTreeMap::from([("identity".to_string(), None)]),
            "ana",
            now,
        )
        .unwrap();
        assert!(cleared.is_empty());
    }

    #[test]
    fn notices() {
        let today = time::macros::date!(2026 - 09 - 23);
        let n = NoticeInput {
            kind: NoticeKind::Rfe,
            items: vec![" Proof of status ".into(), "".into()],
            message: None,
            due_date: None,
        };
        let (items, due) = check_notice(&n, Status::InReview, today, 87).unwrap();
        assert_eq!(items, ["Proof of status"]);
        assert_eq!(format_date(due), "2026-12-19");
        assert!(check_notice(&n, Status::Open, today, 87).is_err());
        let past = NoticeInput {
            due_date: Some("2026-09-23".into()),
            ..n.clone()
        };
        assert!(check_notice(&past, Status::InReview, today, 87).is_err());
        let empty = NoticeInput {
            items: vec![" ".into()],
            ..n
        };
        assert!(check_notice(&empty, Status::InReview, today, 87).is_err());
    }

    #[test]
    fn extensions_and_rescissions() {
        let today = time::macros::date!(2026 - 09 - 23);
        let due = time::macros::date!(2026 - 10 - 01);
        let ext = |d: &str, r: &str| ExtensionInput {
            due_date: d.into(),
            reason: r.into(),
        };
        let (d, r) =
            check_extension(&ext("2026-10-31", " applicant request "), due, today).unwrap();
        assert_eq!(
            (format_date(d).as_str(), r.as_str()),
            ("2026-10-31", "applicant request")
        );
        let err = check_extension(&ext("2026-10-01", ""), due, today).unwrap_err();
        assert_eq!(
            err.fields.keys().collect::<Vec<_>>(),
            ["due_date", "reason"]
        );
        // Overdue notice: the new date must still be in the future.
        let past_due = time::macros::date!(2026 - 09 - 01);
        assert!(check_extension(&ext("2026-09-20", "late request"), past_due, today).is_err());
        assert!(check_extension(&ext("2026-09-24", "late request"), past_due, today).is_ok());
        assert!(check_extension(&ext("31/10/2026", "x"), due, today).is_err());

        assert!(check_rescission("issued in error").is_err());
        assert_eq!(
            check_rescission("  issued to the wrong case in error ").unwrap(),
            "issued to the wrong case in error"
        );
    }
}

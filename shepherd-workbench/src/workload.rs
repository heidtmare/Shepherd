//! Workload rules: roles, worker and rule validation, worker selection, request checks.
//! Pure functions; persistence is in `workload_db`, the assignment engine in `workload_ops`,
//! HTTP in `workload_api`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;

use crate::{benefits::Benefits, case::Status, receipt::Rejection};

/// Role (in the `X-Roles` header) allowed to manage workers, rules and assignments.
pub const MANAGER_ROLE: &str = "manager";
pub const MAX_CASES_LIMIT: i32 = 1000;
const MAX_POOL: usize = 500;
const MAX_TEXT: usize = 2000;

/// Comma-separated role list contains `role` (case-insensitive).
pub fn has_role(roles: Option<&str>, role: &str) -> bool {
    roles.is_some_and(|r| r.split(',').any(|x| x.trim().eq_ignore_ascii_case(role)))
}

/// User names as accepted in `X-User`: trimmed, 1 to 64 bytes.
pub fn normalize_worker(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty() && s.len() <= 64).then(|| s.to_owned())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// Add the case to the requester's workload.
    Add,
    /// Remove the case from the requester's workload.
    Remove,
}

impl RequestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RequestKind::Add => "add",
            RequestKind::Remove => "remove",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "add" => Some(RequestKind::Add),
            "remove" => Some(RequestKind::Remove),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Pending,
    Approved,
    Denied,
    /// Withdrawn by the requester.
    Withdrawn,
    /// Closed by the system: the case was decided.
    Cancelled,
}

impl RequestStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RequestStatus::Pending => "pending",
            RequestStatus::Approved => "approved",
            RequestStatus::Denied => "denied",
            RequestStatus::Withdrawn => "withdrawn",
            RequestStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => RequestStatus::Pending,
            "approved" => RequestStatus::Approved,
            "denied" => RequestStatus::Denied,
            "withdrawn" => RequestStatus::Withdrawn,
            "cancelled" => RequestStatus::Cancelled,
            _ => return None,
        })
    }
}

fn reject(fields: BTreeMap<String, String>) -> Rejection {
    Rejection {
        message: format!("{} value(s) rejected", fields.len()),
        fields,
    }
}

pub fn one(field: &str, problem: impl Into<String>) -> Rejection {
    let problem = problem.into();
    Rejection {
        message: problem.clone(),
        fields: BTreeMap::from([(field.to_owned(), problem)]),
    }
}

fn check_skills(
    benefits: &Benefits,
    skills: &[String],
    p: &mut BTreeMap<String, String>,
) -> Vec<String> {
    let mut out: Vec<String> = skills.iter().map(|s| s.trim().to_owned()).collect();
    for s in &out {
        if benefits.get(s).is_none() {
            p.insert(format!("skills.{s}"), "not a benefit form type".into());
        }
    }
    out.sort();
    out.dedup();
    out
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct WorkerInput {
    /// Defaults to `WORKLOAD_DEFAULT_MAX_CASES`.
    pub max_cases: Option<i32>,
    /// Defaults to true.
    pub auto_assign: Option<bool>,
    /// Benefit form types the worker may adjudicate.
    #[serde(default)]
    pub skills: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkerSettings {
    pub max_cases: i32,
    pub auto_assign: bool,
    pub skills: Vec<String>,
}

pub fn check_worker(
    benefits: &Benefits,
    w: &WorkerInput,
    default_max: i32,
) -> Result<WorkerSettings, Rejection> {
    let mut p = BTreeMap::new();
    let max_cases = w.max_cases.unwrap_or(default_max);
    if !(0..=MAX_CASES_LIMIT).contains(&max_cases) {
        p.insert("max_cases".into(), format!("0 to {MAX_CASES_LIMIT}"));
    }
    let skills = check_skills(benefits, &w.skills, &mut p);
    if p.is_empty() {
        Ok(WorkerSettings {
            max_cases,
            auto_assign: w.auto_assign.unwrap_or(true),
            skills,
        })
    } else {
        Err(reject(p))
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RuleInput {
    pub name: String,
    /// Benefit form type whose unassigned cases the rule distributes.
    pub form_type: String,
    /// Eligible workers; empty = every worker skilled for the form type.
    #[serde(default)]
    pub workers: Vec<String>,
    /// Lower runs first. Default 100.
    pub priority: Option<i32>,
    /// Default true.
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuleSettings {
    pub name: String,
    pub form_type: String,
    pub workers: Vec<String>,
    pub priority: i32,
    pub enabled: bool,
}

pub fn check_rule(benefits: &Benefits, r: &RuleInput) -> Result<RuleSettings, Rejection> {
    let mut p = BTreeMap::new();
    let name = r.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > 200 {
        p.insert("name".into(), "1 to 200 characters".into());
    }
    let form_type = r.form_type.trim().to_owned();
    if benefits.get(&form_type).is_none() {
        p.insert("form_type".into(), "not a benefit form type".into());
    }
    let mut workers = Vec::new();
    for w in &r.workers {
        match normalize_worker(w) {
            Some(n) => workers.push(n),
            None => {
                p.insert("workers".into(), "names are 1 to 64 characters".into());
            }
        }
    }
    workers.sort();
    workers.dedup();
    if workers.len() > MAX_POOL {
        p.insert("workers".into(), format!("at most {MAX_POOL}"));
    }
    let priority = r.priority.unwrap_or(100);
    if !(0..=10_000).contains(&priority) {
        p.insert("priority".into(), "0 to 10000".into());
    }
    if p.is_empty() {
        Ok(RuleSettings {
            name,
            form_type,
            workers,
            priority,
            enabled: r.enabled.unwrap_or(true),
        })
    } else {
        Err(reject(p))
    }
}

/// Worker eligible for an automatic assignment.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Candidate {
    pub worker: String,
    /// Undecided cases assigned.
    pub load: i64,
    pub max_cases: i32,
    pub last_assigned_at: Option<OffsetDateTime>,
}

impl Candidate {
    pub fn has_capacity(&self) -> bool {
        self.load < i64::from(self.max_cases)
    }
}

/// Worker with capacity and the lowest load relative to capacity; ties go to the worker assigned
/// least recently, then by name.
pub fn pick(candidates: &[Candidate]) -> Option<&Candidate> {
    candidates
        .iter()
        .filter(|c| c.has_capacity())
        .min_by(|a, b| {
            // a.load / a.max vs b.load / b.max without division; max > 0 given capacity.
            (i128::from(a.load) * i128::from(b.max_cases))
                .cmp(&(i128::from(b.load) * i128::from(a.max_cases)))
                .then_with(|| a.last_assigned_at.cmp(&b.last_assigned_at))
                .then_with(|| a.worker.cmp(&b.worker))
        })
}

/// Why a case cannot be assigned to a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NotAWorker,
    /// Not skilled for the case's form type.
    Unskilled,
    /// At or over `max_cases`; a manager may pass `force`.
    OverCapacity {
        load: i64,
        max_cases: i32,
    },
}

impl Refusal {
    pub fn message(&self, worker: &str, form_type: &str) -> String {
        match self {
            Refusal::NotAWorker => format!("{worker} is not a registered worker"),
            Refusal::Unskilled => format!("{worker} is not skilled for {form_type}"),
            Refusal::OverCapacity { load, max_cases } => {
                format!("{worker} has {load} of {max_cases} cases")
            }
        }
    }
}

/// Worker state checked before an assignment. `load` excludes the case being assigned.
pub struct Eligibility {
    pub registered: bool,
    pub skilled: bool,
    pub load: i64,
    pub max_cases: i32,
}

/// Skill is always required; capacity only without `force`.
pub fn check_assignment(e: &Eligibility, force: bool) -> Result<(), Refusal> {
    if !e.registered {
        return Err(Refusal::NotAWorker);
    }
    if !e.skilled {
        return Err(Refusal::Unskilled);
    }
    if !force && e.load >= i64::from(e.max_cases) {
        return Err(Refusal::OverCapacity {
            load: e.load,
            max_cases: e.max_cases,
        });
    }
    Ok(())
}

/// Reason text: trimmed, empty = none.
pub fn clean_text(field: &str, s: Option<&str>) -> Result<Option<String>, Rejection> {
    let t = s.map(str::trim).filter(|t| !t.is_empty());
    match t {
        Some(t) if t.chars().count() > MAX_TEXT => {
            Err(one(field, format!("longer than {MAX_TEXT} characters")))
        }
        t => Ok(t.map(str::to_owned)),
    }
}

/// Whether `worker` may request `kind` for a case in `status` assigned to `assignee`.
/// Skill for additions is checked separately.
pub fn check_request(
    kind: RequestKind,
    status: Status,
    assignee: Option<&str>,
    worker: &str,
) -> Result<(), String> {
    if status == Status::Decided {
        return Err("case is decided".into());
    }
    match (kind, assignee) {
        (RequestKind::Add, Some(a)) if a == worker => Err("case is already assigned to you".into()),
        (RequestKind::Remove, Some(a)) if a == worker => Ok(()),
        (RequestKind::Remove, _) => Err("case is not assigned to you".into()),
        (RequestKind::Add, _) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::benefits::{Benefit, Ground};

    fn benefits() -> Benefits {
        Benefits {
            e_notification: None,
            grounds: vec![Ground {
                id: "ineligible".into(),
                label: "Ineligible".into(),
            }],
            benefits: ["uscis.i-765", "uscis.i-130"]
                .iter()
                .map(|f| Benefit {
                    form_type: f.to_string(),
                    name: f.to_string(),
                    facts: Default::default(),
                    highlights: vec![],
                    criteria: vec![],
                })
                .collect(),
        }
    }

    fn cand(worker: &str, load: i64, max: i32, last: Option<i64>) -> Candidate {
        Candidate {
            worker: worker.into(),
            load,
            max_cases: max,
            last_assigned_at: last.map(|s| OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(s)),
        }
    }

    #[test]
    fn roles() {
        assert!(has_role(Some("worker, Manager"), MANAGER_ROLE));
        assert!(!has_role(Some("managers"), MANAGER_ROLE));
        assert!(!has_role(None, MANAGER_ROLE));
    }

    #[test]
    fn pick_prefers_lowest_utilization() {
        let c = [
            cand("ana", 5, 10, Some(1)),
            cand("ben", 2, 2, None),
            cand("cy", 1, 4, Some(5)),
        ];
        assert_eq!(pick(&c).unwrap().worker, "cy");
        let tie = [cand("ana", 1, 4, Some(9)), cand("ben", 2, 8, Some(3))];
        assert_eq!(pick(&tie).unwrap().worker, "ben");
        let never = [cand("ana", 0, 4, Some(1)), cand("ben", 0, 4, None)];
        assert_eq!(pick(&never).unwrap().worker, "ben");
        let full = [cand("ana", 3, 3, None), cand("ben", 0, 0, None)];
        assert!(pick(&full).is_none());
    }

    #[test]
    fn assignment_requires_skill_and_capacity() {
        let e = |registered, skilled, load| Eligibility {
            registered,
            skilled,
            load,
            max_cases: 2,
        };
        assert_eq!(
            check_assignment(&e(false, true, 0), true),
            Err(Refusal::NotAWorker)
        );
        assert_eq!(
            check_assignment(&e(true, false, 0), true),
            Err(Refusal::Unskilled)
        );
        assert_eq!(
            check_assignment(&e(true, true, 2), false),
            Err(Refusal::OverCapacity {
                load: 2,
                max_cases: 2
            })
        );
        assert!(check_assignment(&e(true, true, 2), true).is_ok());
        assert!(check_assignment(&e(true, true, 1), false).is_ok());
    }

    #[test]
    fn worker_and_rule_validation() {
        let b = benefits();
        let w = check_worker(
            &b,
            &WorkerInput {
                max_cases: None,
                auto_assign: None,
                skills: vec![" uscis.i-765".into(), "uscis.i-765".into()],
            },
            10,
        )
        .unwrap();
        assert_eq!(
            w,
            WorkerSettings {
                max_cases: 10,
                auto_assign: true,
                skills: vec!["uscis.i-765".into()]
            }
        );
        let err = check_worker(
            &b,
            &WorkerInput {
                max_cases: Some(-1),
                auto_assign: None,
                skills: vec!["uscis.g-1145".into()],
            },
            10,
        )
        .unwrap_err();
        assert_eq!(
            err.fields.keys().collect::<Vec<_>>(),
            ["max_cases", "skills.uscis.g-1145"]
        );

        let r = check_rule(
            &b,
            &RuleInput {
                name: " EAD ".into(),
                form_type: "uscis.i-765".into(),
                workers: vec!["ben".into(), " ana ".into(), "ben".into()],
                priority: None,
                enabled: None,
            },
        )
        .unwrap();
        assert_eq!(r.name, "EAD");
        assert_eq!(r.workers, ["ana", "ben"]);
        assert_eq!((r.priority, r.enabled), (100, true));
        let err = check_rule(
            &b,
            &RuleInput {
                name: "".into(),
                form_type: "nope".into(),
                workers: vec![" ".into()],
                priority: Some(-5),
                enabled: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.fields.len(), 4);
    }

    #[test]
    fn requests() {
        use RequestKind::*;
        assert!(check_request(Add, Status::Open, None, "ana").is_ok());
        assert!(check_request(Add, Status::InReview, Some("ben"), "ana").is_ok());
        assert!(check_request(Add, Status::InReview, Some("ana"), "ana").is_err());
        assert!(check_request(Add, Status::Decided, None, "ana").is_err());
        assert!(check_request(Remove, Status::AwaitingResponse, Some("ana"), "ana").is_ok());
        assert!(check_request(Remove, Status::InReview, Some("ben"), "ana").is_err());
        assert!(check_request(Remove, Status::Open, None, "ana").is_err());
    }
}

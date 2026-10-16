//! Assignment engine: rule dispatch of unassigned cases, refilling a worker's workload, the
//! periodic sweep, and the checked assignment used by managers and approved requests.
//!
//! Every assignment change runs in a transaction holding [`workload_db::lock_assignments`], so a
//! worker's load cannot change between the capacity check and the write.

use std::{collections::BTreeMap, time::Duration};

use serde_json::json;
use shepherd_common::events::{CASE_ASSIGNED, CaseAssigned};
use sqlx::{PgConnection, PgPool};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    case::{Assessment, Status},
    case_db::{self, Case},
    projection::Projector,
    workload::{self, Eligibility, Refusal},
    workload_db,
};

/// Unassigned cases dispatched per sweep.
const SWEEP_BATCH: i64 = 500;

/// How an assignment was made.
#[derive(Debug, Clone, Copy)]
pub enum Via {
    Rule(Uuid),
    Manager,
    Request(Uuid),
    Reopen,
    /// The case's form type changed to one the assignee is not skilled for.
    FormType,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Rule(_) => "rule",
            Via::Manager => "manager",
            Via::Request(_) => "request",
            Via::Reopen => "reopen",
            Via::FormType => "form_type",
        }
    }
}

/// Committed assignee change, published after commit.
#[derive(Debug, Clone)]
pub struct Change {
    pub case_id: Uuid,
    pub document_id: Uuid,
    pub receipt_number: String,
    pub form_type: String,
    pub assignee: Option<String>,
    pub previous: Option<String>,
    pub by: Option<String>,
    pub via: Via,
}

impl Change {
    pub fn reopened(c: &Case, by: &str) -> Self {
        Change {
            case_id: c.id,
            document_id: c.primary_document_id,
            receipt_number: c.receipt_number.clone(),
            form_type: c.form_type.clone(),
            assignee: Some(by.to_owned()),
            previous: c.assignee.clone(),
            by: Some(by.to_owned()),
            via: Via::Reopen,
        }
    }
}

/// Sets or clears the assignee of an undecided case and logs it. The caller holds the assignment
/// lock and the case row lock. A worker whose case a manager or removal request returns to the
/// pool is excluded from rule assignment of it; one it is given to explicitly is no longer.
pub async fn set(
    conn: &mut PgConnection,
    c: &Case,
    assignee: Option<&str>,
    by: Option<&str>,
    via: Via,
) -> sqlx::Result<Option<Change>> {
    if c.assignee.as_deref() == assignee {
        return Ok(None);
    }
    let next = match Status::parse(&c.status) {
        Some(Status::AwaitingResponse) => Status::AwaitingResponse,
        _ => Status::working(assignee),
    };
    case_db::set_assignee(conn, c.id, assignee, next.as_str()).await?;
    if matches!(via, Via::Manager | Via::Request(_)) {
        match (assignee, &c.assignee) {
            (Some(a), _) => workload_db::unexclude(conn, c.id, a).await?,
            (None, Some(prev)) => workload_db::exclude(conn, c.id, prev).await?,
            (None, None) => {}
        }
    }
    let (rule_id, request_id) = match via {
        Via::Rule(r) => (Some(r), None),
        Via::Request(q) => (None, Some(q)),
        _ => (None, None),
    };
    let action = if assignee.is_some() {
        "assigned"
    } else {
        "unassigned"
    };
    case_db::log(
        conn,
        c.id,
        by,
        action,
        json!({ "assignee": assignee, "previous": c.assignee, "via": via.as_str(),
                "rule_id": rule_id, "request_id": request_id }),
    )
    .await?;
    Ok(Some(Change {
        case_id: c.id,
        document_id: c.primary_document_id,
        receipt_number: c.receipt_number.clone(),
        form_type: c.form_type.clone(),
        assignee: assignee.map(str::to_owned),
        previous: c.assignee.clone(),
        by: by.map(str::to_owned),
        via,
    }))
}

/// Worker state for assigning case `c` to `worker`.
pub async fn eligibility(
    conn: &mut PgConnection,
    c: &Case,
    worker: &str,
) -> sqlx::Result<Eligibility> {
    Ok(match workload_db::worker(conn, worker).await? {
        None => Eligibility {
            registered: false,
            skilled: false,
            load: 0,
            max_cases: 0,
        },
        Some(w) => Eligibility {
            registered: true,
            skilled: w.skills.iter().any(|s| s == &c.form_type),
            load: workload_db::load(conn, worker, Some(c.id)).await?,
            max_cases: w.max_cases,
        },
    })
}

/// Assigns a locked, undecided case to `worker` after checking skill and (without `force`)
/// capacity. The caller holds the assignment lock.
pub async fn assign_checked(
    conn: &mut PgConnection,
    c: &Case,
    worker: &str,
    force: bool,
    by: &str,
    via: Via,
) -> sqlx::Result<Result<Option<Change>, Refusal>> {
    let e = eligibility(conn, c, worker).await?;
    if let Err(r) = workload::check_assignment(&e, force) {
        return Ok(Err(r));
    }
    Ok(Ok(set(conn, c, Some(worker), Some(by), via).await?))
}

/// Result of [`retype`].
pub struct Retyped {
    /// The case after the change.
    pub case: Case,
    /// Assessments of the previous benefit, removed.
    pub cleared: BTreeMap<String, Assessment>,
    /// The assignee was unassigned: not skilled for the new form type.
    pub change: Option<Change>,
}

/// Changes the form type of a locked, undecided case. Criterion assessments are cleared, including
/// those whose ids the new benefit shares (criteria are defined per benefit). An assignee not
/// skilled for the new form type is unassigned. The caller holds the assignment lock and the case
/// row lock, logs the change, and dispatches the case after commit.
pub async fn retype(
    conn: &mut PgConnection,
    c: &Case,
    to: &str,
    by: Option<&str>,
) -> sqlx::Result<Retyped> {
    case_db::set_form_type(conn, c.id, to).await?;
    let cleared = c.criteria.0.clone();
    if !cleared.is_empty() {
        case_db::set_criteria(conn, c.id, &BTreeMap::new()).await?;
    }
    let mut case = c.clone();
    case.form_type = to.to_owned();
    case.criteria.0.clear();
    let change = match case.assignee.clone() {
        Some(a) if !eligibility(conn, &case, &a).await?.skilled => {
            set(conn, &case, None, by, Via::FormType).await?
        }
        _ => None,
    };
    if change.is_some() {
        case.assignee = None;
        if case.status != Status::AwaitingResponse.as_str() {
            case.status = Status::Open.as_str().to_owned();
        }
    }
    Ok(Retyped {
        case,
        cleared,
        change,
    })
}

/// Gives an open, unassigned case to the least-loaded eligible worker of the first matching rule.
pub async fn dispatch_locked(conn: &mut PgConnection, c: &Case) -> sqlx::Result<Option<Change>> {
    if c.status != Status::Open.as_str() || c.assignee.is_some() {
        return Ok(None);
    }
    for rule in workload_db::matching_rules(conn, &c.form_type).await? {
        let candidates = workload_db::candidates(conn, c.id, &c.form_type, &rule.workers).await?;
        if let Some(w) = workload::pick(&candidates) {
            let worker = w.worker.clone();
            return set(conn, c, Some(&worker), None, Via::Rule(rule.id)).await;
        }
    }
    Ok(None)
}

#[derive(Clone)]
pub struct Assigner {
    pub db: PgPool,
    pub js: async_nats::jetstream::Context,
    pub projector: Projector,
}

impl Assigner {
    /// Re-projects changed cases and publishes `case.assigned`.
    pub async fn published(&self, changes: &[Change]) {
        for ch in changes {
            self.projector.spawn_sync(ch.case_id);
            metrics::counter!("case_assignments_total", "via" => ch.via.as_str()).increment(1);
            let rule_id = match ch.via {
                Via::Rule(r) => Some(r),
                _ => None,
            };
            crate::publish(
                &self.js,
                CASE_ASSIGNED,
                ch.document_id,
                CaseAssigned {
                    case_id: ch.case_id,
                    receipt_number: ch.receipt_number.clone(),
                    form_type: ch.form_type.clone(),
                    assignee: ch.assignee.clone(),
                    previous: ch.previous.clone(),
                    assigned_by: ch.by.clone(),
                    via: ch.via.as_str().into(),
                    rule_id,
                },
            )
            .await;
        }
    }

    /// Assigns one case by rule, if it is open and unassigned.
    pub async fn dispatch(&self, case_id: Uuid) -> anyhow::Result<Option<String>> {
        let mut tx = self.db.begin().await?;
        workload_db::lock_assignments(&mut tx).await?;
        let Some(c) = case_db::lock(&mut tx, case_id).await? else {
            return Ok(None);
        };
        let ch = dispatch_locked(&mut tx, &c).await?;
        tx.commit().await?;
        let assignee = ch.as_ref().and_then(|c| c.assignee.clone());
        self.published(ch.as_slice()).await;
        Ok(assignee)
    }

    /// Fills `worker`'s workload up to `max_cases` from the rule backlog.
    pub async fn fill(&self, worker: &str) -> anyhow::Result<Vec<Uuid>> {
        let mut tx = self.db.begin().await?;
        workload_db::lock_assignments(&mut tx).await?;
        let Some(w) = workload_db::worker(&mut tx, worker).await? else {
            return Ok(vec![]);
        };
        let room = i64::from(w.max_cases) - w.load;
        if !w.auto_assign || room <= 0 {
            return Ok(vec![]);
        }
        let mut changes = Vec::new();
        for b in workload_db::backlog_for(&mut tx, worker, room).await? {
            let Some(c) = case_db::lock(&mut tx, b.case_id).await? else {
                continue;
            };
            if let Some(ch) = set(&mut tx, &c, Some(worker), None, Via::Rule(b.rule_id)).await? {
                changes.push(ch);
            }
        }
        tx.commit().await?;
        self.published(&changes).await;
        Ok(changes.iter().map(|c| c.case_id).collect())
    }

    /// Dispatches the rule backlog, oldest first. Returns the number of cases assigned.
    pub async fn sweep(&self) -> anyhow::Result<usize> {
        let mut n = 0;
        for id in workload_db::backlog(&self.db, SWEEP_BATCH).await? {
            if self.dispatch(id).await?.is_some() {
                n += 1;
            }
        }
        Ok(n)
    }

    pub fn spawn_dispatch(&self, case_id: Uuid) {
        let a = self.clone();
        tokio::spawn(async move {
            if let Err(e) = a.dispatch(case_id).await {
                tracing::warn!(error = format!("{e:#}"), %case_id, "rule assignment deferred");
            }
        });
    }

    /// Dispatches `case_id` (if it became unassigned), then refills `worker`.
    pub fn spawn_rebalance(&self, case_id: Option<Uuid>, workers: Vec<String>) {
        let a = self.clone();
        tokio::spawn(async move {
            if let Some(id) = case_id
                && let Err(e) = a.dispatch(id).await
            {
                tracing::warn!(error = format!("{e:#}"), case_id = %id, "rule assignment deferred");
            }
            for w in workers {
                if let Err(e) = a.fill(&w).await {
                    tracing::warn!(error = format!("{e:#}"), worker = %w, "workload fill deferred");
                }
            }
        });
    }

    pub fn spawn_sweep(&self) {
        let a = self.clone();
        tokio::spawn(async move {
            if let Err(e) = a.sweep().await {
                tracing::warn!(error = format!("{e:#}"), "workload sweep failed");
            }
        });
    }

    /// Periodic sweep: catches cases whose immediate dispatch failed and capacity freed without
    /// an event (e.g. a raised `max_cases`).
    pub async fn run(self, every: Duration, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(every) => {}
            }
            match self.sweep().await {
                Ok(0) => {}
                Ok(n) => tracing::info!(assigned = n, "workload sweep"),
                Err(e) => tracing::warn!(error = format!("{e:#}"), "workload sweep failed"),
            }
        }
    }
}

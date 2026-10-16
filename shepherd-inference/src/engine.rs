//! Rule evaluation.
//!
//! Each `workbench.case.v1` projection replaces the case's parties in the identity index, then
//! the case is evaluated together with every case whose findings can change with it: cases that
//! share an A-Number with it before or after the change, and cases with an open finding naming it.
//! An evaluation holds advisory locks on the cases it evaluates and on their A-Numbers, so
//! concurrent changes to linked cases cannot leave stale findings while unrelated cases evaluate
//! in parallel. A-Numbers held by more than `A_NUMBER_CASE_LIMIT` cases are not joined: other
//! holders' parties are not loaded and changes are not fanned out to them.
//!
//! Findings are diffed against the open findings of the case: new detections are raised, changed
//! ones updated, missing ones resolved. `inference.findings.v1` (the open findings) is written to
//! the case's primary document after each change; a loop retries writes that failed. When the
//! primary document changes, workbench writes a retraction (`retracted: true`) on the former one:
//! findings move to the new primary and the former primary's findings artifact is emptied.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use shepherd_common::{
    events::{FINDING_RAISED, FINDING_RESOLVED, FindingRaised, FindingResolved},
    storage_client::{DocumentDetail, StorageClient},
};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    PRODUCER, SERVICE,
    db::{self, Finding, NewFinding, PartyFacts, Snapshot},
    rules::{Configured, Context, Detection, Party, RuleSet},
};

pub const CASE_SCHEMA: &str = "workbench.case.v1";
pub const FINDINGS_SCHEMA: &str = "inference.findings.v1";
const FINGERPRINT_KEY: &str = "rules_fingerprint";
/// Attempts to settle the lock set of a transaction before giving up (the event is retried).
const LOCK_ATTEMPTS: usize = 5;

#[derive(Debug, Default, Deserialize)]
struct ProjectedParty {
    family_name: Option<String>,
    given_name: Option<String>,
    a_number: Option<String>,
    date_of_birth: Option<String>,
}

/// The fields of `workbench.case.v1` used here.
#[derive(Debug, Deserialize)]
pub struct CaseProjection {
    pub case_id: Uuid,
    pub receipt_number: String,
    pub form_type: String,
    pub status: String,
    pub revision: i32,
    #[serde(default)]
    applicant: ProjectedParty,
    beneficiary: Option<ProjectedParty>,
}

fn present(v: &Option<String>) -> Option<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// `workbench.case.v1` retraction on a former primary document.
#[derive(Debug, Deserialize)]
struct Retraction {
    case_id: Uuid,
    primary_document_id: Uuid,
}

/// `inference.findings.v1` on a former primary document.
#[derive(Debug, Serialize)]
struct RetractedFindings {
    schema: &'static str,
    case_id: Uuid,
    retracted: bool,
    primary_document_id: Uuid,
    findings: [(); 0],
}

impl CaseProjection {
    /// Parties with at least one value.
    pub fn parties(&self) -> Vec<PartyFacts> {
        [
            ("applicant", Some(&self.applicant)),
            ("beneficiary", self.beneficiary.as_ref()),
        ]
        .into_iter()
        .filter_map(|(role, p)| {
            let p = p?;
            let f = PartyFacts {
                role,
                family_name: present(&p.family_name),
                given_name: present(&p.given_name),
                a_number: present(&p.a_number),
                date_of_birth: present(&p.date_of_birth),
            };
            (f != PartyFacts {
                role,
                ..Default::default()
            })
            .then_some(f)
        })
        .collect()
    }
}

/// Changes to one case's open findings.
#[derive(Default)]
pub struct Plan<'a> {
    pub raise: Vec<(&'a Configured, &'a Detection)>,
    /// Finding, new values, clear the review.
    pub update: Vec<(&'a Finding, &'a Configured, &'a Detection, bool)>,
    pub resolve: Vec<&'a Finding>,
}

impl Plan<'_> {
    pub fn is_empty(&self) -> bool {
        self.raise.is_empty() && self.update.is_empty() && self.resolve.is_empty()
    }
}

pub fn plan<'a>(open: &'a [Finding], detections: &'a [(&'a Configured, Detection)]) -> Plan<'a> {
    let mut p = Plan::default();
    let mut seen = BTreeSet::new();
    for (rule, d) in detections {
        if !seen.insert((rule.def.id, d.key.as_str())) {
            continue;
        }
        match open
            .iter()
            .find(|f| f.rule == rule.def.id && f.key == d.key)
        {
            None => p.raise.push((rule, d)),
            Some(f) => {
                let related_changed = f.related_cases != d.related_cases;
                if related_changed
                    || f.severity != rule.severity.as_str()
                    || f.summary != d.summary
                    || f.evidence.0 != d.evidence
                {
                    p.update.push((f, rule, d, related_changed));
                }
            }
        }
    }
    p.resolve = open
        .iter()
        .filter(|f| !seen.contains(&(f.rule.as_str(), f.key.as_str())))
        .collect();
    p
}

/// An event to publish once the transaction commits.
struct Pending {
    kind: &'static str,
    document_id: Uuid,
    data: Value,
}

#[derive(Default)]
struct Outcome {
    events: Vec<Pending>,
    changed: Vec<Uuid>,
}

/// Advisory lock keys of a transaction. Hash collisions only serialize unrelated work.
#[derive(Default)]
struct Locks(BTreeSet<i64>);

impl Locks {
    fn add(&mut self, kind: &str, id: &str) {
        let h = Sha256::digest(format!("inference.{kind}:{id}"));
        self.0
            .insert(i64::from_be_bytes(h[..8].try_into().expect("8 bytes")));
    }

    fn case(&mut self, id: Uuid) {
        self.add("case", &id.to_string());
    }

    fn a_numbers(&mut self, a: &[String]) {
        for x in a {
            self.add("a_number", x);
        }
    }
}

/// What a transaction evaluates.
enum Target<'a> {
    /// A projection of the case with these A-Numbers, sorted.
    Ingest {
        case_id: Uuid,
        a_numbers: &'a [String],
    },
    /// One case, as stored.
    Case(Uuid),
}

/// Locks a target needs, and the other cases it re-evaluates.
struct Needs {
    locks: Locks,
    affected: Vec<Uuid>,
}

/// Whether the values other cases' rules see are unchanged: receipt number, form type, parties.
fn same_identity(c: &db::CaseRow, parties: &[Party], s: &Snapshot) -> bool {
    c.receipt_number == s.receipt_number
        && c.form_type == s.form_type
        && parties.len() == s.parties.len()
        && parties.iter().zip(s.parties).all(|(p, f)| {
            p.role == f.role
                && p.family_name == f.family_name
                && p.given_name == f.given_name
                && p.a_number == f.a_number
                && p.date_of_birth == f.date_of_birth
        })
}

#[derive(Clone)]
pub struct Engine {
    pub db: PgPool,
    pub storage: StorageClient,
    pub js: async_nats::jetstream::Context,
    pub rules: Arc<RuleSet>,
    /// An A-Number held by more cases than this is not joined across cases.
    pub a_number_case_limit: i64,
    /// Rules fingerprint and `a_number_case_limit`; a change re-evaluates every case.
    pub fingerprint: String,
    /// Background `inference.findings.v1` writes in flight.
    writes: Arc<Semaphore>,
}

fn new_finding<'a>(case_id: Uuid, rule: &'a Configured, d: &'a Detection) -> NewFinding<'a> {
    NewFinding {
        rule: rule.def.id,
        case_id,
        key: &d.key,
        severity: rule.severity.as_str(),
        summary: &d.summary,
        evidence: &d.evidence,
        related_cases: &d.related_cases,
    }
}

impl Engine {
    pub fn new(
        db: PgPool,
        storage: StorageClient,
        js: async_nats::jetstream::Context,
        rules: Arc<RuleSet>,
        a_number_case_limit: i64,
        write_concurrency: usize,
    ) -> Self {
        let fingerprint = hex::encode(Sha256::digest(format!(
            "{}\na_number_case_limit={a_number_case_limit}",
            rules.fingerprint
        )));
        Self {
            db,
            storage,
            js,
            rules,
            a_number_case_limit,
            fingerprint,
            writes: Arc::new(Semaphore::new(write_concurrency.max(1))),
        }
    }

    /// Reads the locks `t` needs. Unlocked reads are an estimate; `begin_locked` confirms them.
    async fn needs(&self, conn: &mut PgConnection, t: &Target<'_>) -> sqlx::Result<Needs> {
        let mut locks = Locks::default();
        match *t {
            Target::Case(id) => {
                locks.case(id);
                locks.a_numbers(&db::a_numbers(conn, &[id]).await?);
                Ok(Needs {
                    locks,
                    affected: vec![],
                })
            }
            Target::Ingest {
                case_id,
                a_numbers: new,
            } => {
                let old = db::a_numbers(conn, &[case_id]).await?;
                let mut keys: Vec<String> = old.iter().chain(new).cloned().collect();
                keys.sort();
                keys.dedup();
                // Other holders of an A-Number over the limit both before and after the change
                // are not re-evaluated: their findings on it do not depend on this case.
                let limit = self.a_number_case_limit;
                let over = |others: i64, held: bool| others + i64::from(held) > limit;
                let fan_out: Vec<String> = db::case_counts(conn, &keys, Some(case_id), limit + 1)
                    .await?
                    .into_iter()
                    .filter(|(k, n)| !(over(*n, old.contains(k)) && over(*n, new.contains(k))))
                    .map(|(k, _)| k)
                    .collect();
                let affected = db::affected(conn, case_id, &fan_out).await?;
                locks.case(case_id);
                locks.a_numbers(&keys);
                for &id in &affected {
                    locks.case(id);
                }
                locks.a_numbers(&db::a_numbers(conn, &affected).await?);
                Ok(Needs { locks, affected })
            }
        }
    }

    /// Opens a transaction holding the locks `t` needs. The needs are read again under the locks;
    /// if they grew in between, the transaction restarts holding the union.
    async fn begin_locked(
        &self,
        t: &Target<'_>,
    ) -> anyhow::Result<(Transaction<'static, Postgres>, Needs)> {
        let mut held = self.needs(&mut *self.db.acquire().await?, t).await?.locks;
        for _ in 0..LOCK_ATTEMPTS {
            let mut tx = self.db.begin().await?;
            db::lock_keys(&mut tx, &held.0).await?;
            let n = self.needs(&mut tx, t).await?;
            if n.locks.0.is_subset(&held.0) {
                return Ok((tx, n));
            }
            metrics::counter!("inference_lock_retries_total").increment(1);
            held.0.extend(n.locks.0);
        }
        anyhow::bail!("lock set still growing after {LOCK_ATTEMPTS} attempts")
    }

    /// Applies a case projection artifact. Older revisions than the stored one are ignored, and so
    /// is an artifact no longer the document's latest projection: the newer one's event follows
    /// (storage publishes events from an outbox).
    pub async fn ingest(&self, document_id: Uuid, artifact_id: Uuid) -> anyhow::Result<()> {
        let doc = self.storage.document(document_id).await?;
        if doc.latest(CASE_SCHEMA).is_some_and(|a| a.id != artifact_id) {
            metrics::counter!("inference_projections_superseded_total").increment(1);
            return Ok(());
        }
        let body: Value = serde_json::from_slice(&self.storage.content(artifact_id).await?)?;
        if body["retracted"] == true {
            return self
                .retract(&doc, artifact_id, serde_json::from_value(body)?)
                .await;
        }
        let p: CaseProjection = serde_json::from_value(body)?;
        let parties = p.parties();
        let mut a_numbers: Vec<String> =
            parties.iter().filter_map(|f| f.a_number.clone()).collect();
        a_numbers.sort();
        a_numbers.dedup();
        let (mut tx, needs) = self
            .begin_locked(&Target::Ingest {
                case_id: p.case_id,
                a_numbers: &a_numbers,
            })
            .await?;
        let before = db::case(&mut tx, p.case_id).await?;
        let before_parties = db::parties(&mut tx, p.case_id).await?;
        let snapshot = Snapshot {
            case_id: p.case_id,
            receipt_number: &p.receipt_number,
            form_type: &p.form_type,
            status: &p.status,
            document_id,
            case_artifact_id: artifact_id,
            case_revision: p.revision,
            parties: &parties,
        };
        if !db::upsert_case(&mut tx, &snapshot).await? {
            tx.commit().await?;
            return Ok(());
        }
        let mut out = Outcome::default();
        self.evaluate(&mut tx, p.case_id, &mut out).await?;
        // Status, due dates and documents do not reach other cases' rules.
        if before.is_none_or(|c| !same_identity(&c, &before_parties, &snapshot)) {
            for &id in &needs.affected {
                self.evaluate(&mut tx, id, &mut out).await?;
            }
        }
        tx.commit().await?;
        // The primary document may have changed; `write` is a no-op when current.
        if !out.changed.contains(&p.case_id) {
            out.changed.push(p.case_id);
        }
        self.finish(out).await;
        Ok(())
    }

    /// The case moved from `document_id` to a new primary document.
    async fn retract(
        &self,
        doc: &DocumentDetail,
        artifact_id: Uuid,
        r: Retraction,
    ) -> anyhow::Result<()> {
        let document_id = doc.id;
        db::move_document(&self.db, r.case_id, document_id, r.primary_document_id).await?;
        if let Some(a) = doc.latest(FINDINGS_SCHEMA) {
            let current: Value = serde_json::from_slice(&self.storage.content(a.id).await?)?;
            if current["retracted"] != true {
                let body = serde_json::to_vec(&RetractedFindings {
                    schema: FINDINGS_SCHEMA,
                    case_id: r.case_id,
                    retracted: true,
                    primary_document_id: r.primary_document_id,
                    findings: [],
                })?;
                self.storage
                    .create_artifact_as(
                        document_id,
                        FINDINGS_SCHEMA,
                        PRODUCER,
                        artifact_id,
                        SERVICE,
                        body,
                    )
                    .await?;
            }
        }
        self.spawn_write(r.case_id);
        Ok(())
    }

    async fn evaluate(
        &self,
        conn: &mut PgConnection,
        case_id: Uuid,
        out: &mut Outcome,
    ) -> anyhow::Result<()> {
        let Some(row) = db::case(conn, case_id).await? else {
            return Ok(());
        };
        let parties = db::parties(conn, case_id).await?;
        let mut a_numbers: Vec<String> =
            parties.iter().filter_map(|p| p.a_number.clone()).collect();
        a_numbers.sort();
        a_numbers.dedup();
        let limit = self.a_number_case_limit;
        let (mut over_limit, mut joined) = (Vec::new(), Vec::new());
        for (k, n) in db::case_counts(conn, &a_numbers, None, limit + 1).await? {
            if n > limit {
                over_limit.push(k);
            } else {
                joined.push(k);
            }
        }
        let related = db::related(conn, case_id, &joined).await?;
        let case = row.case_ref();
        let detections = self.rules.evaluate(&Context {
            case: &case,
            parties: &parties,
            related: &related,
            over_limit: &over_limit,
            case_limit: limit,
        });
        let open = db::open_findings(conn, case_id).await?;
        let plan = plan(&open, &detections);
        for (rule, d) in &plan.raise {
            let id = db::insert_finding(conn, &new_finding(case_id, rule, d)).await?;
            metrics::counter!("inference_findings_raised_total", "rule" => rule.def.id)
                .increment(1);
            tracing::info!(finding_id = %id, case_id = %case_id, rule = rule.def.id, "finding raised");
            out.events.push(Pending {
                kind: FINDING_RAISED,
                document_id: row.document_id,
                data: serde_json::to_value(FindingRaised {
                    finding_id: id,
                    case_id,
                    receipt_number: row.receipt_number.clone(),
                    rule: rule.def.id.to_owned(),
                    severity: rule.severity.as_str().to_owned(),
                    related_cases: d.related_cases.clone(),
                })?,
            });
        }
        for (f, rule, d, clear_review) in &plan.update {
            db::update_finding(conn, f.id, &new_finding(case_id, rule, d), *clear_review).await?;
        }
        for f in &plan.resolve {
            db::resolve_finding(conn, f.id).await?;
            metrics::counter!("inference_findings_resolved_total", "rule" => f.rule.clone())
                .increment(1);
            tracing::info!(finding_id = %f.id, case_id = %case_id, rule = %f.rule, "finding resolved");
            out.events.push(Pending {
                kind: FINDING_RESOLVED,
                document_id: row.document_id,
                data: serde_json::to_value(FindingResolved {
                    finding_id: f.id,
                    case_id,
                    receipt_number: row.receipt_number.clone(),
                    rule: f.rule.clone(),
                })?,
            });
        }
        let changed = !plan.is_empty();
        db::mark_evaluated(conn, case_id, changed).await?;
        metrics::counter!("inference_evaluations_total").increment(1);
        if changed {
            out.changed.push(case_id);
        }
        Ok(())
    }

    async fn finish(&self, out: Outcome) {
        for e in out.events {
            crate::publish(&self.js, e.kind, e.document_id, e.data).await;
        }
        for id in out.changed {
            self.spawn_write(id);
        }
    }

    /// Re-evaluates every case with the current rules. Each case is its own transaction.
    pub async fn evaluate_all(&self, shutdown: &CancellationToken) -> anyhow::Result<usize> {
        let (mut after, mut n) = (None, 0);
        loop {
            let ids = db::case_ids_after(&self.db, after, 500).await?;
            let Some(last) = ids.last().copied() else {
                return Ok(n);
            };
            for id in ids {
                if shutdown.is_cancelled() {
                    anyhow::bail!("shutdown");
                }
                let (mut tx, _) = self.begin_locked(&Target::Case(id)).await?;
                let mut out = Outcome::default();
                self.evaluate(&mut tx, id, &mut out).await?;
                tx.commit().await?;
                self.finish(out).await;
                n += 1;
            }
            after = Some(last);
        }
    }

    /// Re-evaluates every case when the rules changed since the last full evaluation.
    pub async fn reconcile(&self, shutdown: CancellationToken) {
        let res = async {
            if db::meta(&self.db, FINGERPRINT_KEY).await?.as_deref()
                == Some(self.fingerprint.as_str())
            {
                return anyhow::Ok(());
            }
            tracing::info!(fingerprint = %self.fingerprint, "rules changed; re-evaluating all cases");
            let n = self.evaluate_all(&shutdown).await?;
            db::set_meta(&self.db, FINGERPRINT_KEY, &self.fingerprint).await?;
            tracing::info!(cases = n, "re-evaluation complete");
            Ok(())
        }
        .await;
        if let Err(e) = res {
            tracing::warn!(
                error = format!("{e:#}"),
                "re-evaluation incomplete; retried at next start"
            );
        }
    }

    /// Writes `inference.findings.v1` if the stored one is stale.
    pub async fn write(&self, case_id: Uuid) -> anyhow::Result<()> {
        let Some((c, findings)) = db::artifact_state(&self.db, case_id).await? else {
            return Ok(());
        };
        if c.written_revision >= c.findings_revision {
            return Ok(());
        }
        let body = serde_json::to_vec(&FindingsArtifact::build(&c, &findings))?;
        let art = self
            .storage
            .create_artifact_as(
                c.document_id,
                FINDINGS_SCHEMA,
                PRODUCER,
                c.case_artifact_id,
                SERVICE,
                body,
            )
            .await?;
        db::mark_written(&self.db, case_id, c.findings_revision, art.id).await?;
        Ok(())
    }

    /// Writes in the background when a write slot is free; otherwise the retry loop writes it.
    pub fn spawn_write(&self, case_id: Uuid) {
        let Ok(permit) = self.writes.clone().try_acquire_owned() else {
            metrics::counter!("inference_writes_deferred_total").increment(1);
            return;
        };
        let e = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(err) = e.write(case_id).await {
                tracing::warn!(error = format!("{err:#}"), case_id = %case_id, "findings artifact deferred");
            }
        });
    }

    /// Writes findings artifacts that were deferred or failed. A full page written without
    /// failure is followed by the next at once.
    pub async fn run_writer(self, every: Duration, shutdown: CancellationToken) {
        const PAGE: i64 = 100;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(every) => {}
            }
            loop {
                let ids = match db::unwritten(&self.db, PAGE).await {
                    Ok(ids) => ids,
                    Err(e) => {
                        tracing::warn!(error = %e, "unwritten findings scan failed");
                        break;
                    }
                };
                let full = ids.len() as i64 == PAGE;
                let mut failed = false;
                for id in ids {
                    if shutdown.is_cancelled() {
                        return;
                    }
                    if let Err(e) = self.write(id).await {
                        failed = true;
                        tracing::warn!(error = format!("{e:#}"), case_id = %id, "findings artifact failed");
                    }
                }
                if !full || failed {
                    break;
                }
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct ArtifactFinding<'a> {
    id: Uuid,
    rule: &'a str,
    key: &'a str,
    severity: &'a str,
    summary: &'a str,
    evidence: &'a Value,
    related_cases: &'a [Uuid],
    review: Option<&'a str>,
    #[serde(with = "time::serde::rfc3339")]
    raised_at: time::OffsetDateTime,
}

/// `inference.findings.v1`: open findings of a case, excluding dismissed ones.
#[derive(Debug, Serialize)]
struct FindingsArtifact<'a> {
    schema: &'static str,
    case_id: Uuid,
    receipt_number: &'a str,
    case_artifact_id: Uuid,
    revision: i32,
    findings: Vec<ArtifactFinding<'a>>,
    dismissed: usize,
}

impl<'a> FindingsArtifact<'a> {
    fn build(c: &'a db::CaseRow, findings: &'a [Finding]) -> Self {
        let dismissed = |f: &&Finding| f.review.as_deref() == Some("dismissed");
        Self {
            schema: FINDINGS_SCHEMA,
            case_id: c.case_id,
            receipt_number: &c.receipt_number,
            case_artifact_id: c.case_artifact_id,
            revision: c.findings_revision,
            findings: findings
                .iter()
                .filter(|f| !dismissed(f))
                .map(|f| ArtifactFinding {
                    id: f.id,
                    rule: &f.rule,
                    key: &f.key,
                    severity: &f.severity,
                    summary: &f.summary,
                    evidence: &f.evidence.0,
                    related_cases: &f.related_cases,
                    review: f.review.as_deref(),
                    raised_at: f.raised_at,
                })
                .collect(),
            dismissed: findings.iter().filter(dismissed).count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::types::Json;

    use super::*;

    fn finding(rule: &str, key: &str, related: Vec<Uuid>, summary: &str) -> Finding {
        let now = time::OffsetDateTime::now_utc();
        Finding {
            id: Uuid::now_v7(),
            rule: rule.into(),
            case_id: Uuid::now_v7(),
            key: key.into(),
            severity: "high".into(),
            status: "open".into(),
            summary: summary.into(),
            evidence: Json(json!({})),
            related_cases: related,
            review: Some("dismissed".into()),
            reviewed_by: Some("ana".into()),
            reviewed_at: Some(now),
            review_note: None,
            raised_at: now,
            updated_at: now,
            resolved_at: None,
        }
    }

    fn detection(key: &str, related: Vec<Uuid>, summary: &str) -> Detection {
        Detection {
            key: key.into(),
            summary: summary.into(),
            evidence: json!({}),
            related_cases: related,
        }
    }

    #[test]
    fn plans_raise_update_resolve() {
        let rules = RuleSet::parse(include_str!("../config/rules.toml")).unwrap();
        let configured = rules.configured("a_number_name_mismatch").unwrap();
        let id = "a_number_name_mismatch";
        let (x, y) = (Uuid::now_v7(), Uuid::now_v7());
        let open = vec![
            finding(id, "111111111", vec![x], "same"),
            finding(id, "222222222", vec![x], "old"),
            finding(id, "333333333", vec![x], "gone"),
            finding(id, "444444444", vec![x], "reworded"),
        ];
        let detections = vec![
            (configured, detection("111111111", vec![x], "same")),
            (configured, detection("222222222", vec![x, y], "old")),
            (configured, detection("444444444", vec![x], "new wording")),
            (configured, detection("555555555", vec![y], "new")),
            (configured, detection("555555555", vec![y], "duplicate")),
        ];
        let p = plan(&open, &detections);
        assert_eq!(p.raise.len(), 1);
        assert_eq!(p.raise[0].1.summary, "new");
        let updates: Vec<_> = p
            .update
            .iter()
            .map(|(f, _, _, c)| (f.key.as_str(), *c))
            .collect();
        assert_eq!(updates, [("222222222", true), ("444444444", false)]);
        assert_eq!(p.resolve.len(), 1);
        assert_eq!(p.resolve[0].key, "333333333");
    }

    #[test]
    fn projection_parties() {
        let p: CaseProjection = serde_json::from_value(json!({
            "schema": "workbench.case.v1", "case_id": Uuid::now_v7(), "receipt_number": "SHP2600000001",
            "form_type": "uscis.i-130", "status": "open", "revision": 3, "received_at": "2026-09-01",
            "applicant": { "family_name": "Smith", "given_name": " ", "a_number": "087654321", "date_of_birth": null },
            "beneficiary": { "family_name": null, "given_name": null, "a_number": null, "date_of_birth": null },
            "documents": [],
        }))
        .unwrap();
        let parties = p.parties();
        assert_eq!(parties.len(), 1);
        assert_eq!(parties[0].role, "applicant");
        assert_eq!(parties[0].given_name, None);
        assert_eq!(parties[0].a_number.as_deref(), Some("087654321"));
    }
}

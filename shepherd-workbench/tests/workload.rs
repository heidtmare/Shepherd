//! Workloads: rule assignment by skill and capacity, worker requests, manager approval and
//! reassignment, refill on completion. Real Postgres + NATS, in-process storage and
//! workbench; cases are opened on stored PDFs without recognition.
//! `cargo test -p shepherd-workbench -- --ignored workload`.

use std::{path::PathBuf, sync::Arc, time::Duration};

use object_store::{ObjectStore, memory::InMemory};
use reqwest::{Method as M, StatusCode, header};
use serde_json::{Value, json};
use shepherd_common::events::{CASE_ASSIGNED, EVENTS_STREAM, subject};
use shepherd_storage::testing::{Infra, Running, spawn_storage, wait_ready};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const I765: &str = "uscis.i-765";

fn path(p: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(p)
        .to_string_lossy()
        .into_owned()
}

struct Client {
    http: reqwest::Client,
    adj: String,
    storage: String,
}

impl Client {
    fn req(&self, m: M, p: &str, user: &str) -> reqwest::RequestBuilder {
        self.http
            .request(m, format!("{}{p}", self.adj))
            .header("x-user", user)
    }

    fn manager(&self, m: M, p: &str) -> reqwest::RequestBuilder {
        self.req(m, p, "mia").header("x-roles", "manager")
    }

    async fn ok(&self, r: reqwest::RequestBuilder) -> anyhow::Result<Value> {
        let r = r.send().await?;
        let status = r.status();
        let body: Value = r.json().await.unwrap_or(Value::Null);
        anyhow::ensure!(status.is_success(), "{status}: {body}");
        Ok(body)
    }

    async fn status(&self, r: reqwest::RequestBuilder) -> anyhow::Result<(StatusCode, Value)> {
        let r = r.send().await?;
        let s = r.status();
        Ok((s, r.json().await.unwrap_or(Value::Null)))
    }

    async fn case(&self, id: &str) -> anyhow::Result<Value> {
        Ok(self
            .ok(self.req(M::GET, &format!("/v1/cases/{id}"), "mia"))
            .await?["case"]
            .clone())
    }

    /// Stores a PDF and opens an I-765 case on it.
    async fn open(&self, received: &str) -> anyhow::Result<String> {
        let doc: Value = self
            .http
            .post(format!("{}/v1/documents", self.storage))
            .query(&[("filename", format!("i765-{received}.pdf"))])
            .header(header::CONTENT_TYPE, "application/pdf")
            .body(include_bytes!("../../fixtures/sample.pdf").to_vec())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let c = self
            .ok(self.req(M::POST, "/v1/cases", "mia").json(&json!({
                "document_id": doc["document_id"], "form_type": I765, "received_date": received
            })))
            .await?;
        Ok(c["id"].as_str().unwrap().to_owned())
    }

    /// Polls until the case has `assignee` (`None` = unassigned).
    async fn until_assignee(&self, id: &str, assignee: Option<&str>) -> anyhow::Result<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let c = self.case(id).await?;
            if c["assignee"].as_str() == assignee {
                return Ok(c);
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "case {id}: assignee {} != {assignee:?}",
                c["assignee"]
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn withdraw(&self, id: &str, user: &str) -> anyhow::Result<Value> {
        self.ok(self
            .req(M::POST, &format!("/v1/cases/{id}/decision"), user)
            .json(&json!({ "outcome": "withdrawn", "rationale": "Applicant withdrew by letter." })))
            .await
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and vendored pdfium"]
async fn workload() -> anyhow::Result<()> {
    let infra = Infra::start().await?;
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let storage = spawn_storage(infra.storage_config(), store).await?;
    let cfg = shepherd_workbench::Config {
        bind_addr: "127.0.0.1:0".into(),
        database_url: infra.url("workbench_rw", "workbench"),
        nats_url: infra.nats_url.clone(),
        storage_url: storage.url(),
        intake_url: None,
        forms_dir: path("forms"),
        pdfium_lib_dir: path("vendor/pdfium/lib"),
        page_dpi: 60.0,
        claim_ttl_minutes: 30,
        open_unrecognized: true,
        db_max_connections: 5,
        search_url: None,
        notification_url: None,
        inference_url: None,
        benefits_path: path("shepherd-workbench/config/benefits.toml"),
        receipt_prefix: "SHP".into(),
        rfe_response_days: 87,
        noid_response_days: 33,
        case_sync_secs: 1,
        workload_default_max_cases: 10,
        workload_sweep_secs: 3600,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let shutdown = CancellationToken::new();
    let adj = Running {
        addr,
        shutdown: shutdown.clone(),
        handle: tokio::spawn(shepherd_workbench::run(cfg, listener, shutdown)),
    };
    wait_ready(addr, "/readyz").await?;
    let c = Client {
        http: reqwest::Client::new(),
        adj: adj.url(),
        storage: storage.url(),
    };

    // Managing workers and rules requires the manager role.
    let (s, _) = c
        .status(
            c.req(M::PUT, "/v1/workload/workers/ana", "ana")
                .json(&json!({ "skills": [I765] })),
        )
        .await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, body) = c
        .status(
            c.manager(M::PUT, "/v1/workload/workers/ana")
                .json(&json!({ "skills": ["uscis.g-1145"] })),
        )
        .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    for (w, skills, max) in [
        ("ana", json!([I765]), 2),
        ("ben", json!([I765]), 1),
        ("cy", json!(["uscis.i-130"]), 5),
    ] {
        c.ok(c
            .manager(M::PUT, &format!("/v1/workload/workers/{w}"))
            .json(&json!({ "skills": skills, "max_cases": max })))
            .await?;
    }

    // Cases opened without a rule stay unassigned.
    let mut opened = Vec::new();
    for d in ["2026-09-01", "2026-09-02", "2026-09-03", "2026-09-04"] {
        opened.push(c.open(d).await?);
    }
    assert_eq!(c.case(&opened[0]).await?["status"], "open");

    // A rule distributes the backlog to the least-utilized skilled worker, up to each worker's
    // max_cases: ana 2, ben 1, one case waits. (Which case waits depends on whether the dispatch
    // spawned when each case opened runs before or after the rule exists.)
    let (s, _) = c
        .status(
            c.manager(M::POST, "/v1/workload/rules")
                .json(&json!({ "name": "EAD", "form_type": "nope" })),
        )
        .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let rule = c
        .ok(c
            .manager(M::POST, "/v1/workload/rules")
            .json(&json!({ "name": "EAD", "form_type": I765 })))
        .await?;
    c.ok(c.manager(M::POST, "/v1/workload/sweep")).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let (ana_cases, ben_cases, waiting) = loop {
        let mut by: [Vec<String>; 3] = Default::default();
        for id in &opened {
            let k = match c.case(id).await?["assignee"].as_str() {
                Some("ana") => 0,
                Some("ben") => 1,
                _ => 2,
            };
            by[k].push(id.clone());
        }
        if by[0].len() == 2 && by[1].len() == 1 {
            let [a, b, w] = by;
            break (a, b, w);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "rule assignment: {by:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let (c1, c3, c2, c4) = (
        ana_cases[0].clone(),
        ana_cases[1].clone(),
        ben_cases[0].clone(),
        waiting[0].clone(),
    );
    assert_eq!(c.case(&c1).await?["status"], "in_review");
    let hist = c
        .ok(c.req(M::GET, &format!("/v1/cases/{c1}/history"), "mia"))
        .await?;
    let assigned = hist
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"] == "assigned")
        .unwrap();
    assert_eq!(assigned["details"]["via"], "rule");
    assert_eq!(assigned["details"]["rule_id"], rule["id"]);

    // Only the assignee adjudicates; workers cannot assign themselves.
    let (s, _) = c
        .status(
            c.req(M::PUT, &format!("/v1/cases/{c1}/criteria"), "ben")
                .json(&json!({ "criteria": {} })),
        )
        .await?;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = c
        .status(c.req(M::POST, &format!("/v1/cases/{c4}/assign"), "ben"))
        .await?;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // c1, c3: ana; c2: ben; c4: unassigned.
    // Requests to add c4. Unskilled workers cannot ask; duplicates are refused.
    let add = json!({ "kind": "add", "reason": "Related to a case I hold" });
    let ana_req = c
        .ok(c
            .req(M::POST, &format!("/v1/cases/{c4}/requests"), "ana")
            .json(&add))
        .await?;
    assert_eq!(ana_req["status"], "pending");
    let (s, body) = c
        .status(
            c.req(M::POST, &format!("/v1/cases/{c4}/requests"), "ana")
                .json(&add),
        )
        .await?;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(body["request_id"], ana_req["id"]);
    let (s, _) = c
        .status(
            c.req(M::POST, &format!("/v1/cases/{c4}/requests"), "cy")
                .json(&add),
        )
        .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let ben_req = c
        .ok(c
            .req(M::POST, &format!("/v1/cases/{c4}/requests"), "ben")
            .json(&add))
        .await?;
    let pending = c.ok(c.req(M::GET, "/v1/workload/requests", "mia")).await?;
    assert_eq!(pending.as_array().unwrap().len(), 2);

    // Approval respects capacity unless forced.
    let rid = ana_req["id"].as_str().unwrap();
    let (s, body) = c
        .status(c.manager(M::POST, &format!("/v1/workload/requests/{rid}/approve")))
        .await?;
    assert_eq!(
        (s, body["error"].as_str()),
        (StatusCode::CONFLICT, Some("over_capacity"))
    );
    let (s, _) = c
        .status(c.req(
            M::POST,
            &format!("/v1/workload/requests/{rid}/approve"),
            "ana",
        ))
        .await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let approved = c
        .ok(c
            .manager(M::POST, &format!("/v1/workload/requests/{rid}/approve"))
            .json(&json!({ "force": true, "note": "Keep related filings together" })))
        .await?;
    assert_eq!(approved["status"], "approved");
    c.until_assignee(&c4, Some("ana")).await?;
    let bid = ben_req["id"].as_str().unwrap();
    let denied = c
        .ok(c
            .manager(M::POST, &format!("/v1/workload/requests/{bid}/deny"))
            .json(&json!({ "note": "Assigned to ana" })))
        .await?;
    assert_eq!(denied["status"], "denied");
    let (s, _) = c
        .status(c.manager(M::POST, &format!("/v1/workload/requests/{bid}/approve")))
        .await?;
    assert_eq!(s, StatusCode::CONFLICT);

    // Removal returns c2 to the pool; rules do not give it back to ben, and ana is full (3/2).
    let (s, _) = c
        .status(
            c.req(M::POST, &format!("/v1/cases/{c2}/requests"), "ana")
                .json(&json!({ "kind": "remove" })),
        )
        .await?;
    assert_eq!(s, StatusCode::CONFLICT);
    let rm = c
        .ok(c
            .req(M::POST, &format!("/v1/cases/{c2}/requests"), "ben")
            .json(&json!({ "kind": "remove", "reason": "Conflict of interest" })))
        .await?;
    let rid = rm["id"].as_str().unwrap();
    c.ok(c.manager(M::POST, &format!("/v1/workload/requests/{rid}/approve")))
        .await?;
    c.until_assignee(&c2, None).await?;
    let swept = c.ok(c.manager(M::POST, "/v1/workload/sweep")).await?;
    assert_eq!(swept["assigned"], 0);
    assert_eq!(c.case(&c2).await?["status"], "open");

    // Completing a case refills the worker only while under max_cases: after c1 ana holds 2/2,
    // after c3 she holds 1/2 and receives c2.
    c.withdraw(&c1, "ana").await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(c.case(&c2).await?["assignee"], Value::Null);
    c.withdraw(&c3, "ana").await?;
    c.until_assignee(&c2, Some("ana")).await?;
    let ana = c
        .ok(c.req(M::GET, "/v1/workload/workers/me", "ana"))
        .await?;
    assert_eq!(
        (ana["load"].as_i64(), ana["max_cases"].as_i64()),
        (Some(2), Some(2))
    );

    // Bulk reassignment is all or nothing and checks the receiver's capacity.
    let move_both = json!({ "case_ids": [c2, c4], "assignee": "ben" });
    let (s, _) = c
        .status(c.manager(M::POST, "/v1/workload/reassign").json(&move_both))
        .await?;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(c.case(&c2).await?["assignee"], "ana");
    let (s, _) = c
        .status(
            c.manager(M::POST, "/v1/workload/reassign")
                .json(&json!({ "case_ids": [c2], "assignee": "cy" })),
        )
        .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let moved = c
        .ok(c.manager(M::POST, "/v1/workload/reassign").json(&json!({
            "case_ids": [c2, c4], "assignee": "ben", "force": true
        })))
        .await?;
    assert_eq!(moved["changed"].as_array().unwrap().len(), 2);
    assert_eq!(c.case(&c4).await?["assignee"], "ben");

    // A skill cannot be removed, nor a worker deleted, while cases are held.
    let (s, _) = c
        .status(
            c.manager(M::PUT, "/v1/workload/workers/ben")
                .json(&json!({ "skills": ["uscis.i-130"], "max_cases": 1 })),
        )
        .await?;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = c
        .status(c.manager(M::DELETE, "/v1/workload/workers/ben"))
        .await?;
    assert_eq!(s, StatusCode::CONFLICT);

    // Unassigning returns c4 to the pool; ben is excluded, ana (0/2) receives it.
    c.ok(c.manager(M::POST, &format!("/v1/cases/{c4}/unassign")))
        .await?;
    c.until_assignee(&c4, Some("ana")).await?;

    // New cases are assigned when opened.
    let c5 = c.open("2026-09-05").await?;
    c.until_assignee(&c5, Some("ana")).await?;

    // Reopening assigns the reopening officer, who must be skilled.
    let (s, _) = c
        .status(
            c.req(M::POST, &format!("/v1/cases/{c1}/reopen"), "cy")
                .json(&json!({ "reason": "Supervisory review of the withdrawal letter" })),
        )
        .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    let workers = c.ok(c.req(M::GET, "/v1/workload/workers", "mia")).await?;
    let load = |w: &str| {
        workers
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["worker"] == w)
            .map(|x| x["load"].as_i64().unwrap())
    };
    assert_eq!(
        (load("ana"), load("ben"), load("cy")),
        (Some(2), Some(1), Some(0))
    );
    let history = c
        .ok(c.req(M::GET, "/v1/workload/requests?status=all&worker=ben", "ben"))
        .await?;
    let states: Vec<&str> = history
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q["status"].as_str().unwrap())
        .collect();
    assert_eq!(states.len(), 2);
    assert!(states.contains(&"denied") && states.contains(&"approved"));

    // A form type change clears criterion assessments and unassigns an assignee not skilled for
    // the new form; the case is dispatched under the new form's rules.
    c.ok(c
        .req(M::PUT, &format!("/v1/cases/{c5}/criteria"), "ana")
        .json(&json!({ "criteria": { "identity": { "result": "met" } } })))
        .await?;
    c.ok(c
        .manager(M::POST, "/v1/workload/rules")
        .json(&json!({ "name": "Family", "form_type": "uscis.i-130" })))
        .await?;
    let edited = c
        .ok(c
            .req(M::PATCH, &format!("/v1/cases/{c5}"), "ana")
            .json(&json!({ "form_type": "uscis.i-130", "reason": "Filed on I-130" })))
        .await?;
    assert_eq!(edited["criteria"], json!({}));
    c.until_assignee(&c5, Some("cy")).await?;
    let hist = c
        .ok(c.req(M::GET, &format!("/v1/cases/{c5}/history"), "mia"))
        .await?;
    let hist = hist.as_array().unwrap();
    let ed = hist.iter().rfind(|e| e["action"] == "edited").unwrap();
    assert_eq!(
        ed["details"]["changes"]["criteria_cleared"]["identity"]["result"],
        "met"
    );
    let un = hist.iter().rfind(|e| e["action"] == "unassigned").unwrap();
    assert_eq!(
        (&un["details"]["previous"], &un["details"]["via"]),
        (&json!("ana"), &json!("form_type"))
    );

    // case.assigned events carry the assignment source.
    let a1 = c.case(&c1).await?;
    let js = async_nats::jetstream::new(async_nats::connect(&infra.nats_url).await?);
    let ev = js
        .get_stream(EVENTS_STREAM)
        .await?
        .get_last_raw_message_by_subject(&subject(
            CASE_ASSIGNED,
            a1["primary_document_id"].as_str().unwrap().parse()?,
        ))
        .await?;
    let ev: Value = serde_json::from_slice(&ev.payload)?;
    assert_eq!(ev["data"]["assignee"], "ana");
    assert_eq!(ev["data"]["via"], "rule");

    adj.stop().await?;
    storage.stop().await
}

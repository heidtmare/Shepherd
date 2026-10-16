//! Typed I-765 upload → case opened → discovered through search → RFE extended, overdue and
//! rescinded → RFE and response → primary document replaced and restored →
//! assessment → form correction → approval → reopen → denial → closure; NOID unanswered →
//! denial for abandonment; NOID → withdrawal. Real Postgres + NATS, in-process
//! storage, search, intake and workbench with vendored pdfium (no TrOCR: typed AcroForm values
//! are read exactly). `cargo test -p shepherd-workbench -- --ignored cases`.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use lopdf::{Document, Object};
use object_store::{ObjectStore, memory::InMemory};
use reqwest::{Method as M, StatusCode, header};
use serde_json::{Value, json};
use shepherd_common::events::{CASE_DECIDED, EVENTS_STREAM, subject};
use shepherd_forms::template::TemplateSet;
use shepherd_storage::testing::{Infra, Running, memory_blob_config, spawn_storage, wait_ready};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const RECEIPT: &str = "IOE2600000042";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn path(p: &str) -> String {
    root().join(p).to_string_lossy().into_owned()
}

async fn spawn<F, Fut>(f: F) -> anyhow::Result<Running>
where
    F: FnOnce(TcpListener, CancellationToken) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(f(listener, shutdown.clone()));
    wait_ready(addr, "/readyz").await?;
    Ok(Running {
        addr,
        shutdown,
        handle,
    })
}

/// Sets `/V` on the blank's fields, as a PDF typed into in a viewer would have.
fn fill_acroform(blank: &[u8], values: &BTreeMap<String, String>) -> Vec<u8> {
    let mut doc = Document::load_mem(blank).unwrap();
    let raw = shepherd_forms::acroform::read_fields(&doc);
    let ids = shepherd_forms::acroform::widget_ids(&raw);
    let targets: Vec<(String, String)> = raw
        .iter()
        .zip(&ids)
        .filter_map(|(f, id)| values.get(id).map(|v| (f.name.clone(), v.clone())))
        .collect();
    let objects: Vec<_> = doc.objects.keys().copied().collect();
    for oid in objects {
        let Ok(d) = doc.get_dictionary(oid) else {
            continue;
        };
        let Some(t) = d
            .get(b"T")
            .ok()
            .and_then(|o| lopdf::decode_text_string(o).ok())
        else {
            continue;
        };
        if let Some((_, v)) = targets
            .iter()
            .find(|(name, _)| name.ends_with(&t) && name.rsplit('.').next() == Some(t.as_str()))
        {
            doc.get_dictionary_mut(oid)
                .unwrap()
                .set("V", Object::string_literal(v.as_str()));
        }
    }
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

struct Client {
    http: reqwest::Client,
    adj: String,
    intake: String,
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

    async fn get(&self, p: &str) -> anyhow::Result<Value> {
        Ok(self
            .req(M::GET, p, "ana")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn upload(
        &self,
        pdf: Vec<u8>,
        filename: &str,
        receipt: Option<&str>,
    ) -> anyhow::Result<String> {
        let mut q = vec![("filename", filename.to_owned())];
        if let Some(r) = receipt {
            q.push(("receipt_number", r.to_owned()));
            q.push(("received_date", "2026-09-01".to_owned()));
        }
        let up: Value = self
            .http
            .post(format!("{}/v1/intake", self.intake))
            .query(&q)
            .header(header::CONTENT_TYPE, "application/pdf")
            .body(pdf)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(up["document_id"].as_str().unwrap().to_owned())
    }

    async fn search(&self, body: Value) -> anyhow::Result<Value> {
        Ok(self
            .req(M::POST, "/v1/cases/search", "ana")
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Polls `f` until it returns `Some`.
    async fn until<T, F, Fut>(&self, what: &str, f: F) -> anyhow::Result<T>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<Option<T>>>,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(v) = f().await? {
                return Ok(v);
            }
            anyhow::ensure!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// Uploads the typed I-765 with `given_name` under `receipt` and waits for its case; returns the
/// case and primary document ids.
async fn open_case(
    c: &Client,
    blank: &[u8],
    truth: &BTreeMap<String, String>,
    receipt: &str,
    given_name: &str,
) -> anyhow::Result<(String, String)> {
    let mut values = truth.clone();
    values.insert(
        "form1[0].Page1[0].Line1b_GivenName[0]".into(),
        given_name.into(),
    );
    let doc = c
        .upload(
            fill_acroform(blank, &values),
            "i765-typed.pdf",
            Some(receipt),
        )
        .await?;
    let found = c
        .until("case opened", || async {
            let r = c.search(json!({ "q": receipt })).await?;
            Ok(r["exact"].as_array().and_then(|a| a.first().cloned()))
        })
        .await?;
    assert_eq!(found["primary_document_id"], doc.as_str());
    assert_eq!(found["facts"]["given_name"], given_name);
    Ok((found["id"].as_str().unwrap().to_owned(), doc))
}

fn search_cfg(infra: &Infra) -> shepherd_search::Config {
    shepherd_search::Config {
        bind_addr: "127.0.0.1:0".into(),
        database_url: infra.url("search_rw", "search"),
        read_database_url: None,
        nats_url: infra.nats_url.clone(),
        mapping_path: path("shepherd-search/config/mapping.toml"),
        content_source: shepherd_search::config::ContentSourceKind::Blob,
        storage_url: None,
        blob: memory_blob_config(),
        admin_token: None,
        consumer_batch: 64,
        consumer_concurrency: 4,
        query_timeout_ms: 2000,
        rank_window: 2000,
        facet_window: 100_000,
        facet_cache_ttl_ms: 0,
        artifact_cache_mb: 16,
        rebuild_shards: 4,
        rebuild_concurrency: 4,
        query_work_mem: "32MB".into(),
        db_max_connections: 5,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and vendored pdfium"]
async fn case_lifecycle() -> anyhow::Result<()> {
    shepherd_forms::render::init(path("vendor/pdfium/lib"))?;
    let infra = Infra::start().await?;
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let storage = spawn_storage(infra.storage_config(), store.clone()).await?;
    let scfg = search_cfg(&infra);
    let search = spawn(|l, s| shepherd_search::run(scfg, Some(store.clone()), l, s)).await?;
    let intake_cfg = shepherd_intake::Config {
        bind_addr: "127.0.0.1:0".into(),
        nats_url: infra.nats_url.clone(),
        storage_url: storage.url(),
        public_storage_url: storage.url(),
        public_search_url: search.url(),
        public_stream_url: "http://stream.invalid".into(),
        public_workbench_url: "http://workbench.invalid".into(),
        concurrency: 2,
        extract_timeout_secs: 30,
        max_page_chars: 10_000,
        recognition_enabled: true,
        forms_dir: path("forms"),
        pdfium_lib_dir: path("vendor/pdfium/lib"),
        htr_model_dir: path("target/no-htr-model"),
        recognition_concurrency: 1,
        recognition_timeout_secs: 120,
        recognition_blank_cache_mb: 64,
    };
    let intake = spawn(|l, s| shepherd_intake::run(intake_cfg, l, s)).await?;
    let adj_cfg = shepherd_workbench::Config {
        bind_addr: "127.0.0.1:0".into(),
        database_url: infra.url("workbench_rw", "workbench"),
        nats_url: infra.nats_url.clone(),
        storage_url: storage.url(),
        intake_url: Some(intake.url()),
        forms_dir: path("forms"),
        pdfium_lib_dir: path("vendor/pdfium/lib"),
        page_dpi: 60.0,
        claim_ttl_minutes: 30,
        open_unrecognized: true,
        db_max_connections: 5,
        search_url: Some(search.url()),
        notification_url: None,
        inference_url: None,
        benefits_path: path("shepherd-workbench/config/benefits.toml"),
        receipt_prefix: "SHP".into(),
        rfe_response_days: 87,
        noid_response_days: 33,
        case_sync_secs: 1,
        workload_default_max_cases: 10,
        workload_sweep_secs: 60,
    };
    let adj = spawn(|l, s| shepherd_workbench::run(adj_cfg, l, s)).await?;
    let c = Client {
        http: reqwest::Client::new(),
        adj: adj.url(),
        intake: intake.url(),
    };

    // Typed I-765 filed under a receipt number: recognition needs no receipting, a case opens.
    let set = TemplateSet::load(path("forms"))?;
    let t = set.get("uscis.i-765").unwrap();
    let truth: BTreeMap<String, String> =
        serde_json::from_slice(&std::fs::read(path("forms/samples/i-765.values.json"))?)?;
    let pdf = fill_acroform(&std::fs::read(set.blank_path(t))?, &truth);
    let doc = c.upload(pdf, "i765-typed.pdf", Some(RECEIPT)).await?;
    let found = c
        .until("case opened", || async {
            let r = c.search(json!({ "q": RECEIPT })).await?;
            Ok(r["exact"].as_array().and_then(|a| a.first().cloned()))
        })
        .await?;
    let id = found["id"].as_str().unwrap().to_owned();
    assert_eq!(found["form_type"], "uscis.i-765");
    assert_eq!(found["primary_document_id"], doc.as_str());
    assert_eq!(found["received_date"], "2026-09-01");
    assert_eq!(found["facts"]["family_name"], "Nguyen");
    assert_eq!(found["facts"]["a_number"], "123456789");
    assert_eq!(found["facts"]["mailing_street"], "1420 Maple Avenue");
    assert_eq!(found["facts"]["mailing_city"], "Portland");
    assert_eq!(found["facts"]["mailing_zip"], "97205");
    assert_eq!(found["status"], "open");
    assert_eq!(found["open_tasks"], 0);
    let by_a = c.search(json!({ "q": "A-123-456-789" })).await?;
    assert_eq!(by_a["exact"][0]["id"], id.as_str());

    // Discovery through shepherd-search once the projection is indexed.
    let hit = c
        .until("case searchable", || async {
            let r = c
                .search(json!({ "q": "Nguyen", "status": ["open"], "form_type": "uscis.i-765" }))
                .await?;
            Ok((r["source"] == "search"
                && r["cases"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|h| h["case"]["id"] == id.as_str())))
            .then_some(r))
        })
        .await?;
    assert!(hit["facet_counts"]["case_status"].is_array());

    // Evidence labeled with the receipt number is attached as supporting.
    let evidence = c
        .upload(
            include_bytes!("../../fixtures/sample.pdf").to_vec(),
            "passport.pdf",
            Some(RECEIPT),
        )
        .await?;
    c.until("evidence attached", || async {
        let d = c.get(&format!("/v1/cases/{id}")).await?;
        Ok(d["documents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["document_id"] == evidence.as_str() && x["role"] == "supporting")
            .then_some(()))
    })
    .await?;
    // No unrecognized receipting task for labeled evidence.
    let tasks: Vec<Value> = c
        .req(M::GET, &format!("/v1/documents/{evidence}/tasks"), "ana")
        .send()
        .await?
        .json()
        .await?;
    assert!(tasks.is_empty());

    // Decisions need an assignee, and the assignee must be the one deciding. Assignment is a
    // manager action and requires a worker skilled for the form.
    let approve = json!({ "outcome": "approved" });
    let r = c
        .req(M::POST, &format!("/v1/cases/{id}/decision"), "ana")
        .json(&approve)
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let r = c
        .req(M::POST, &format!("/v1/cases/{id}/assign"), "ana")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = c
        .manager(M::POST, &format!("/v1/cases/{id}/assign"))
        .json(&json!({ "assignee": "ana" }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    for w in ["ana", "ben"] {
        c.manager(M::PUT, &format!("/v1/workload/workers/{w}"))
            .json(&json!({ "skills": ["uscis.i-765"] }))
            .send()
            .await?
            .error_for_status()?;
    }
    c.manager(M::POST, &format!("/v1/cases/{id}/assign"))
        .json(&json!({ "assignee": "ana" }))
        .send()
        .await?
        .error_for_status()?;
    let r = c
        .req(M::POST, &format!("/v1/cases/{id}/decision"), "ana")
        .json(&approve)
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let rej: Value = r.json().await?;
    assert!(rej["fields"]["criteria.identity"].is_string());

    // RFE; a labeled upload while awaiting is recorded as the response.
    let r = c
        .req(M::POST, &format!("/v1/cases/{id}/notices"), "ana")
        .json(&json!({ "kind": "rfe", "items": ["Copy of current passport", ""] }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::CREATED);
    let notice: Value = r.json().await?;
    assert_eq!(notice["items"], json!(["Copy of current passport"]));
    let d = c.get(&format!("/v1/cases/{id}")).await?;
    assert_eq!(d["case"]["status"], "awaiting_response");
    assert_eq!(d["form_number"], "I-765");
    assert!(d["e_notification"].is_null());
    let r = c
        .req(M::POST, &format!("/v1/cases/{id}/decision"), "ana")
        .json(&approve)
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    // Only the assignee records responses and attaches documents.
    let notice_id = notice["id"].as_str().unwrap();
    for r in [
        c.req(
            M::POST,
            &format!("/v1/cases/{id}/notices/{notice_id}/response"),
            "ben",
        )
        .json(&json!({ "note": "passport copy received" })),
        c.req(M::POST, &format!("/v1/cases/{id}/documents"), "ben")
            .json(&json!({ "document_id": doc })),
    ] {
        let r = r.send().await?;
        assert_eq!(r.status(), StatusCode::CONFLICT);
        assert_eq!(r.json::<Value>().await?["assignee"], "ana");
    }

    // Extension: assignee only, after the current due date.
    let extension = format!("/v1/cases/{id}/notices/{notice_id}/extension");
    let due = notice["due_date"].as_str().unwrap().to_owned();
    let r = c
        .req(M::POST, &extension, "ben")
        .json(&json!({ "due_date": "2099-01-01", "reason": "applicant request" }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let r = c
        .req(M::POST, &extension, "ana")
        .json(&json!({ "due_date": due, "reason": "applicant request" }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let r = c
        .req(M::POST, &extension, "ana")
        .json(&json!({ "due_date": "2099-01-01", "reason": "applicant request" }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::OK);
    let d = c.get(&format!("/v1/cases/{id}")).await?;
    assert_eq!(d["case"]["response_due"], "2099-01-01");
    assert_eq!(d["notices"][0]["due_date"], "2099-01-01");
    assert_eq!(d["notices"][0]["original_due_date"], due);

    // Overdue worklist: move the due date into the past.
    let overdue = |l: &Value| {
        l["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == id.as_str())
    };
    let l = c.get("/v1/cases?overdue=true").await?;
    assert!(!overdue(&l) && l["overdue"] == 0);
    let pg = sqlx::PgPool::connect(&infra.superuser_url()).await?;
    sqlx::query("UPDATE workbench.cases SET response_due = '2026-01-01' WHERE id = $1::uuid")
        .bind(&id)
        .execute(&pg)
        .await?;
    let l = c.get("/v1/cases?overdue=true").await?;
    assert!(overdue(&l) && l["overdue"] == 1);
    let d = c.get(&format!("/v1/cases/{id}")).await?;
    assert_eq!(d["response_overdue"], true);
    let l = c.get("/v1/cases?overdue=true&status=in_review").await?;
    assert!(l["items"].as_array().unwrap().is_empty());

    // Rescission returns the case to review; a new RFE is then answered.
    let rescind = format!("/v1/cases/{id}/notices/{notice_id}/rescind");
    let r = c
        .req(M::POST, &rescind, "ana")
        .json(&json!({ "reason": "in error" }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let r = c
        .req(M::POST, &rescind, "ana")
        .json(&json!({ "reason": "issued before the evidence on file was reviewed" }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::OK);
    let d = c.get(&format!("/v1/cases/{id}")).await?;
    assert_eq!(d["case"]["status"], "in_review");
    assert!(d["case"]["response_due"].is_null());
    assert!(d["notices"][0]["rescinded_at"].is_string());
    assert_eq!(d["notices"][0]["rescinded_by"], "ana");
    let r = c
        .req(M::POST, &rescind, "ana")
        .json(&json!({ "reason": "issued before the evidence on file was reviewed" }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    assert!(!overdue(&c.get("/v1/cases?overdue=true").await?));
    c.req(M::POST, &format!("/v1/cases/{id}/notices"), "ana")
        .json(&json!({ "kind": "rfe", "items": ["Copy of current passport"] }))
        .send()
        .await?
        .error_for_status()?;

    let response = c
        .upload(
            include_bytes!("../../fixtures/multipage.pdf").to_vec(),
            "rfe-response.pdf",
            Some(RECEIPT),
        )
        .await?;
    c.until("response recorded", || async {
        let d = c.get(&format!("/v1/cases/{id}")).await?;
        Ok((d["case"]["status"] == "in_review").then_some(()))
    })
    .await?;
    let d = c.get(&format!("/v1/cases/{id}")).await?;
    assert!(d["notices"][0]["responded_at"].is_null());
    assert!(d["notices"][1]["responded_at"].is_string());
    assert!(
        d["documents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["document_id"] == response.as_str() && x["role"] == "response")
    );

    // Replace the primary document; search follows it, then restore the typed form.
    let primary = format!("/v1/cases/{id}/primary");
    let make_primary = |user: &str, doc: &str| {
        c.req(M::POST, &primary, user)
            .json(&json!({ "document_id": doc, "reason": "scan attached to the wrong role" }))
    };
    let r = make_primary("ben", &response).send().await?;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let r = make_primary("ana", &doc).send().await?;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let r = make_primary("ana", &response).send().await?;
    assert_eq!(r.status(), StatusCode::OK);
    let d = c.get(&format!("/v1/cases/{id}")).await?;
    assert_eq!(d["case"]["primary_document_id"], response.as_str());
    assert_eq!(d["case"]["form_type"], "uscis.i-765");
    assert_eq!(d["case"]["facts"]["family_name"], "Nguyen");
    let role = |d: &Value, doc: &str| {
        d["documents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["document_id"] == doc)
            .map(|x| x["role"].clone())
    };
    assert_eq!(role(&d, &doc), Some(json!("supporting")));
    assert_eq!(role(&d, &response), Some(json!("primary")));
    c.until("search follows the primary", || async {
        // Case facets are on the primary's card only; the former primary still matches by text.
        let r = c
            .search(json!({ "q": "Nguyen", "status": ["in_review"] }))
            .await?;
        let docs: Vec<Value> = r["cases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|h| h["case"]["id"] == id.as_str())
            .flat_map(|h| h["matches"].as_array().cloned().unwrap_or_default())
            .map(|m| m["document_id"].clone())
            .collect();
        Ok((docs == [json!(response)]).then_some(()))
    })
    .await?;
    make_primary("ana", &doc).send().await?.error_for_status()?;
    let d = c.get(&format!("/v1/cases/{id}")).await?;
    assert_eq!(d["case"]["primary_document_id"], doc.as_str());
    assert_eq!(role(&d, &response), Some(json!("supporting")));

    // Assessment.
    let benefit = &d["benefit"];
    let mut criteria = serde_json::Map::new();
    for cr in benefit["criteria"].as_array().unwrap() {
        let result = if cr["required"] == true { "met" } else { "na" };
        criteria.insert(
            cr["id"].as_str().unwrap().into(),
            json!({ "result": result, "note": "verified" }),
        );
    }
    let r = c
        .req(M::PUT, &format!("/v1/cases/{id}/criteria"), "ben")
        .json(&json!({ "criteria": criteria }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let bad = json!({ "criteria": { "identity": { "result": "na" } } });
    let r = c
        .req(M::PUT, &format!("/v1/cases/{id}/criteria"), "ana")
        .json(&bad)
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    c.req(M::PUT, &format!("/v1/cases/{id}/criteria"), "ana")
        .json(&json!({ "criteria": criteria }))
        .send()
        .await?
        .error_for_status()?;

    // Correct a form value: new receipt (amend) and facts follow the form.
    let given = "form1[0].Page1[0].Line1b_GivenName[0]";
    let amended: Value = c
        .req(
            M::POST,
            &format!("/v1/cases/{id}/documents/{doc}/amend"),
            "ana",
        )
        .json(&json!({ "values": { given: "Thanh-Mai" }, "reason": "Spelling per passport" }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(amended["edited"], 1);
    assert_eq!(amended["task"]["kind"], "amend");
    let form = c.get(&format!("/v1/documents/{doc}/form")).await?;
    assert_eq!(form["receipt"]["mode"], "amend");
    assert_eq!(form["receipt"]["values"][given], "Thanh-Mai");
    assert_eq!(form["receipt"]["amendment_reason"], "Spelling per passport");
    let d = c.get(&format!("/v1/cases/{id}")).await?;
    assert_eq!(d["case"]["facts"]["given_name"], "Thanh-Mai");
    assert_eq!(d["form_final"], true);

    // Officer edit overrides a fact; later form values leave it alone.
    let r = c
        .req(M::PATCH, &format!("/v1/cases/{id}"), "ana")
        .json(&json!({ "facts": { "a_number": "12-345-678" } }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::OK);
    let edited: Value = r.json().await?;
    assert_eq!(edited["facts"]["a_number"], "012345678");
    assert_eq!(edited["overridden"], json!(["a_number"]));
    let r = c
        .req(M::PATCH, &format!("/v1/cases/{id}"), "ana")
        .json(&json!({ "facts": { "date_of_birth": "02/30/1991" } }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Approve.
    let decided: Value = c
        .req(M::POST, &format!("/v1/cases/{id}/decision"), "ana")
        .json(&approve)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(decided["case"]["status"], "decided");
    assert_eq!(decided["case"]["decision"], "approved");
    let first_decision = decided["decision_artifact_id"].as_str().unwrap().to_owned();
    let d = c.get(&format!("/v1/cases/{id}")).await?;
    assert_eq!(d["decision"]["schema"], "workbench.decision.v1");
    assert_eq!(d["decision"]["outcome"], "approved");
    assert_eq!(d["decision"]["facts"]["given_name"], "Thanh-Mai");
    assert_eq!(d["decision"]["notices"].as_array().unwrap().len(), 2);
    assert!(d["decision"]["notices"][0]["rescinded_at"].is_string());
    assert!(
        d["decision"]["criteria"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["result"].is_string())
    );
    let r = c
        .req(M::PATCH, &format!("/v1/cases/{id}"), "ana")
        .json(&json!({ "facts": { "family_name": "Other" } }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let r = c
        .req(M::POST, &format!("/v1/cases/{id}/documents"), "ana")
        .json(&json!({ "document_id": response }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    assert!(
        r.json::<Value>().await?["message"]
            .as_str()
            .unwrap()
            .contains("decided")
    );

    let js = async_nats::jetstream::new(async_nats::connect(&infra.nats_url).await?);
    let ev = js
        .get_stream(EVENTS_STREAM)
        .await?
        .get_last_raw_message_by_subject(&subject(CASE_DECIDED, doc.parse()?))
        .await?;
    let ev: Value = serde_json::from_slice(&ev.payload)?;
    assert_eq!(ev["data"]["outcome"], "approved");
    assert_eq!(ev["data"]["receipt_number"], RECEIPT);

    c.until("decision searchable", || async {
        let r = c.search(json!({ "decision": "approved" })).await?;
        Ok(r["cases"]
            .as_array()
            .is_some_and(|a| a.iter().any(|h| h["case"]["id"] == id.as_str()))
            .then_some(()))
    })
    .await?;

    // Reopen and deny; the new decision supersedes the approval.
    let r = c
        .req(M::POST, &format!("/v1/cases/{id}/reopen"), "ben")
        .json(&json!({ "reason": "short" }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let reopened: Value = c
        .req(M::POST, &format!("/v1/cases/{id}/reopen"), "ben")
        .json(&json!({ "reason": "Supervisory review: category evidence insufficient" }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(
        (reopened["status"].as_str(), reopened["assignee"].as_str()),
        (Some("in_review"), Some("ben"))
    );
    let r = c
        .req(M::POST, &format!("/v1/cases/{id}/decision"), "ben")
        .json(&json!({ "outcome": "denied", "grounds": [], "rationale": "x" }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    c.req(M::POST, &format!("/v1/cases/{id}/decision"), "ben")
        .json(
            &json!({ "outcome": "denied", "grounds": ["insufficient_evidence"],
                       "rationale": "The record does not establish the (c)(8) prerequisite." }),
        )
        .send()
        .await?
        .error_for_status()?;
    let d = c.get(&format!("/v1/cases/{id}")).await?;
    assert_eq!(d["decision"]["outcome"], "denied");
    assert_eq!(d["decision"]["supersedes"], first_decision.as_str());
    assert_eq!(d["decision"]["grounds"][0]["id"], "insufficient_evidence");

    c.req(M::POST, &format!("/v1/cases/{id}/notes"), "ben")
        .json(&json!({ "text": "Denial notice queued." }))
        .send()
        .await?
        .error_for_status()?;
    let hist: Vec<Value> = c
        .req(M::GET, &format!("/v1/cases/{id}/history"), "ana")
        .send()
        .await?
        .json()
        .await?;
    let actions: Vec<&str> = hist.iter().map(|h| h["action"].as_str().unwrap()).collect();
    for a in [
        "opened",
        "document_added",
        "assigned",
        "notice_issued",
        "notice_extended",
        "notice_rescinded",
        "response_received",
        "assessed",
        "amended",
        "edited",
        "decided",
        "reopened",
        "primary_changed",
        "note",
    ] {
        assert!(actions.contains(&a), "{a} missing from {actions:?}");
    }

    // Administrative closure by a manager, of a case assigned to someone else.
    c.req(M::POST, &format!("/v1/cases/{id}/reopen"), "ben")
        .json(&json!({ "reason": "Filing found to duplicate an earlier request" }))
        .send()
        .await?
        .error_for_status()?;
    let close = json!({ "outcome": "closed", "closure_reason": "duplicate",
                        "rationale": "Duplicate of an earlier filing by the same applicant." });
    let r = c
        .req(M::POST, &format!("/v1/cases/{id}/decision"), "ana")
        .json(&close)
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let r = c
        .manager(M::POST, &format!("/v1/cases/{id}/decision"))
        .json(&json!({ "outcome": "closed", "rationale": "Duplicate of an earlier filing by the same applicant." }))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.json::<Value>().await?["fields"]["closure_reason"].is_string());
    c.manager(M::POST, &format!("/v1/cases/{id}/decision"))
        .json(&close)
        .send()
        .await?
        .error_for_status()?;
    let d = c.get(&format!("/v1/cases/{id}")).await?;
    assert_eq!(d["case"]["decision"], "closed");
    assert_eq!(d["case"]["decided_by"], "mia");
    assert_eq!(d["decision"]["closure_reason"], "duplicate");
    c.until("closure searchable", || async {
        let r = c.search(json!({ "decision": "closed" })).await?;
        Ok((r["source"] == "search"
            && r["cases"]
                .as_array()
                .is_some_and(|a| a.iter().any(|h| h["case"]["id"] == id.as_str())))
        .then_some(()))
    })
    .await?;

    let list = c.get("/v1/cases?status=decided").await?;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);

    let blank = std::fs::read(set.blank_path(t))?;
    let last_decided = |doc: String| {
        let js = js.clone();
        async move {
            let ev = js
                .get_stream(EVENTS_STREAM)
                .await?
                .get_last_raw_message_by_subject(&subject(CASE_DECIDED, doc.parse()?))
                .await?;
            anyhow::Ok(serde_json::from_slice::<Value>(&ev.payload)?)
        }
    };
    let assign = |case: &str| {
        c.manager(M::POST, &format!("/v1/cases/{case}/assign"))
            .json(&json!({ "assignee": "ana" }))
    };
    let noid_items = json!({ "kind": "noid",
        "items": ["The record does not show a pending asylum application under (c)(8)."],
        "message": "Submit the receipt notice for Form I-589." });

    // NOID with the default 33-day window, not answered: denial for abandonment only after the
    // due date.
    let (noid_case, noid_doc) = open_case(&c, &blank, &truth, "IOE2600000043", "Minh").await?;
    assign(&noid_case).send().await?.error_for_status()?;
    let noid: Value = c
        .req(M::POST, &format!("/v1/cases/{noid_case}/notices"), "ana")
        .json(&noid_items)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let due = (time::OffsetDateTime::now_utc().date() + time::Duration::days(33))
        .format(time::macros::format_description!("[year]-[month]-[day]"))?;
    assert_eq!(noid["kind"], "noid");
    assert_eq!(noid["due_date"], due.as_str());
    let d = c.get(&format!("/v1/cases/{noid_case}")).await?;
    assert_eq!(d["case"]["status"], "awaiting_response");
    assert_eq!(d["case"]["response_due"], due.as_str());
    assert_eq!(d["notices"][0]["kind"], "noid");
    let decision = format!("/v1/cases/{noid_case}/decision");
    let abandon = json!({ "outcome": "denied", "grounds": ["abandonment"],
        "rationale": "No response to the notice of intent to deny was received by the due date." });
    let r = c
        .req(M::POST, &decision, "ana")
        .json(&abandon)
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let rej: Value = r.json().await?;
    assert!(rej["fields"]["grounds.abandonment"].is_string());
    let r = c
        .req(M::POST, &decision, "ana")
        .json(
            &json!({ "outcome": "denied", "grounds": ["insufficient_evidence"],
            "rationale": "The record does not establish the (c)(8) prerequisite." }),
        )
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.json::<Value>().await?["fields"]["status"].is_string());
    sqlx::query("UPDATE workbench.cases SET response_due = '2026-01-01' WHERE id = $1::uuid")
        .bind(&noid_case)
        .execute(&pg)
        .await?;
    let d = c.get(&format!("/v1/cases/{noid_case}")).await?;
    assert_eq!(d["response_overdue"], true);
    let r = c
        .req(M::POST, &decision, "ben")
        .json(&abandon)
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let decided: Value = c
        .req(M::POST, &decision, "ana")
        .json(&abandon)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(decided["case"]["decision"], "denied");
    assert!(decided["case"]["response_due"].is_null());
    let d = c.get(&format!("/v1/cases/{noid_case}")).await?;
    assert_eq!(d["decision"]["outcome"], "denied");
    assert_eq!(d["decision"]["grounds"][0]["id"], "abandonment");
    assert!(
        d["decision"]["grounds"][0]["label"]
            .as_str()
            .unwrap()
            .starts_with("Abandonment")
    );
    assert_eq!(d["decision"]["notices"][0]["kind"], "noid");
    assert!(d["decision"]["notices"][0]["responded_at"].is_null());
    assert!(d["decision"]["notices"][0]["rescinded_at"].is_null());
    assert!(!overdue(&c.get("/v1/cases?overdue=true").await?));
    let ev = last_decided(noid_doc).await?;
    assert_eq!(ev["data"]["outcome"], "denied");
    assert_eq!(ev["data"]["receipt_number"], "IOE2600000043");

    // NOID outstanding, applicant withdraws: withdrawal needs a rationale and cites no grounds.
    let (wd_case, wd_doc) = open_case(&c, &blank, &truth, "IOE2600000044", "Lan").await?;
    assign(&wd_case).send().await?.error_for_status()?;
    c.req(M::POST, &format!("/v1/cases/{wd_case}/notices"), "ana")
        .json(&noid_items)
        .send()
        .await?
        .error_for_status()?;
    let decision = format!("/v1/cases/{wd_case}/decision");
    let withdraw = json!({ "outcome": "withdrawn",
        "rationale": "Applicant withdrew the request by signed letter." });
    for (body, field) in [
        (
            json!({ "outcome": "withdrawn", "rationale": "withdrawn" }),
            "rationale",
        ),
        (
            json!({ "outcome": "withdrawn", "grounds": ["ineligible"],
                "rationale": "Applicant withdrew the request by signed letter." }),
            "grounds",
        ),
    ] {
        let r = c.req(M::POST, &decision, "ana").json(&body).send().await?;
        assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(r.json::<Value>().await?["fields"][field].is_string());
    }
    let r = c
        .req(M::POST, &decision, "ben")
        .json(&withdraw)
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let decided: Value = c
        .req(M::POST, &decision, "ana")
        .json(&withdraw)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(decided["case"]["status"], "decided");
    assert_eq!(decided["case"]["decision"], "withdrawn");
    assert!(decided["case"]["response_due"].is_null());
    let d = c.get(&format!("/v1/cases/{wd_case}")).await?;
    assert_eq!(d["decision"]["outcome"], "withdrawn");
    assert_eq!(
        d["decision"]["rationale"],
        "Applicant withdrew the request by signed letter."
    );
    assert_eq!(d["decision"]["grounds"], json!([]));
    assert_eq!(d["decision"]["notices"][0]["kind"], "noid");
    let ev = last_decided(wd_doc).await?;
    assert_eq!(ev["data"]["outcome"], "withdrawn");
    c.until("withdrawal searchable", || async {
        let r = c.search(json!({ "decision": "withdrawn" })).await?;
        Ok(r["cases"]
            .as_array()
            .is_some_and(|a| a.iter().any(|h| h["case"]["id"] == wd_case.as_str()))
            .then_some(()))
    })
    .await?;

    let list = c.get("/v1/cases?status=decided").await?;
    assert_eq!(list["items"].as_array().unwrap().len(), 3);

    adj.stop().await?;
    intake.stop().await?;
    search.stop().await?;
    storage.stop().await
}

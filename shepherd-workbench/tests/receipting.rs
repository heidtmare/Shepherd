//! Scan upload → recognition → receipting task → receipt, against real Postgres + NATS with
//! vendored pdfium and TrOCR. `cargo test -p shepherd-workbench --release -- --ignored`.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use object_store::memory::InMemory;
use reqwest::{StatusCode, header};
use serde_json::{Value, json};
use shepherd_common::events::{EVENTS_STREAM, RECEIPTING_COMPLETED, subject};
use shepherd_forms::{render, synth, template::TemplateSet};
use shepherd_storage::testing::{Infra, Running, spawn_storage, wait_ready};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

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

struct Client {
    http: reqwest::Client,
    adj: String,
}

impl Client {
    fn req(&self, m: reqwest::Method, p: &str, user: &str) -> reqwest::RequestBuilder {
        self.http
            .request(m, format!("{}{p}", self.adj))
            .header("x-user", user)
    }

    async fn task_for(&self, doc: &str) -> anyhow::Result<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            let tasks: Vec<Value> = self
                .http
                .get(format!("{}/v1/documents/{doc}/tasks", self.adj))
                .send()
                .await?
                .json()
                .await?;
            if let Some(t) = tasks.into_iter().find(|t| t["status"] == "open") {
                return Ok(t);
            }
            anyhow::ensure!(tokio::time::Instant::now() < deadline, "no task for {doc}");
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker, vendored pdfium and TrOCR"]
async fn scan_to_receipt() -> anyhow::Result<()> {
    render::init(path("vendor/pdfium/lib"))?;
    let infra = Infra::start().await?;
    let storage = spawn_storage(infra.storage_config(), Arc::new(InMemory::new())).await?;
    let intake_cfg = shepherd_intake::Config {
        bind_addr: "127.0.0.1:0".into(),
        nats_url: infra.nats_url.clone(),
        storage_url: storage.url(),
        public_storage_url: storage.url(),
        public_search_url: "http://search.invalid".into(),
        public_stream_url: "http://stream.invalid".into(),
        public_workbench_url: "http://workbench.invalid".into(),
        concurrency: 2,
        extract_timeout_secs: 30,
        max_page_chars: 10_000,
        recognition_enabled: true,
        forms_dir: path("forms"),
        pdfium_lib_dir: path("vendor/pdfium/lib"),
        htr_model_dir: path("vendor/models/trocr-base-handwritten"),
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
        search_url: None,
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
    };
    use reqwest::Method as M;

    // Synthetic handwritten I-765, uploaded through intake.
    let set = TemplateSet::load(path("forms"))?;
    let t = set.get("uscis.i-765").unwrap();
    let truth: BTreeMap<String, String> =
        serde_json::from_slice(&std::fs::read(path("forms/samples/i-765.values.json"))?)?;
    let pdf = synth::synthesize(
        t,
        &std::fs::read(set.blank_path(t))?,
        &truth,
        &std::fs::read(path("forms/fonts/HomemadeApple.ttf"))?,
        &synth::SynthOptions {
            shift: (-5, 8),
            seed: 3,
            ..Default::default()
        },
    )?;
    let up: Value = c
        .http
        .post(format!("{}/v1/intake?filename=i765-scan.pdf", intake.url()))
        .header(header::CONTENT_TYPE, "application/pdf")
        .body(pdf)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let doc = up["document_id"].as_str().unwrap().to_owned();

    let task = c.task_for(&doc).await?;
    let id = task["id"].as_str().unwrap().to_owned();
    assert_eq!(task["form_type"], "uscis.i-765");
    assert_eq!(task["kind"], "flagged");
    assert!(task["flagged_fields"].as_i64().unwrap() > 0);

    let detail: Value = c
        .req(M::GET, &format!("/v1/tasks/{id}"), "ana")
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(detail["pages"].as_array().unwrap().len(), 7);
    assert_eq!(detail["template"]["id"], "uscis.i-765");
    let flagged: Vec<String> = detail["recognition"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["status"] == "requires_receipting")
        .map(|f| f["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        flagged.len() as i64,
        task["flagged_fields"].as_i64().unwrap()
    );

    let png = c
        .http
        .get(format!("{}/v1/documents/{doc}/pages/2", adj.url()))
        .send()
        .await?;
    assert_eq!(png.headers()[header::CONTENT_TYPE], "image/png");
    assert!(png.bytes().await?.starts_with(b"\x89PNG"));

    // Claims are exclusive.
    assert_eq!(
        c.req(M::POST, &format!("/v1/tasks/{id}/claim"), "ana")
            .send()
            .await?
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        c.req(M::POST, &format!("/v1/tasks/{id}/claim"), "ben")
            .send()
            .await?
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        c.http
            .post(format!("{}/v1/tasks/{id}/claim", adj.url()))
            .send()
            .await?
            .status(),
        StatusCode::BAD_REQUEST
    );

    // Review cannot complete while flagged fields are unresolved.
    let r = c
        .req(M::POST, &format!("/v1/tasks/{id}/submit"), "ana")
        .json(&json!({"mode": "review", "values": {}}))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let rej: Value = r.json().await?;
    assert_eq!(rej["fields"].as_object().unwrap().len(), flagged.len());

    // Invalid value is rejected per field.
    let mut values: BTreeMap<String, Value> = flagged
        .iter()
        .map(|f| (f.clone(), json!(truth.get(f))))
        .collect();
    let dob = "form1[0].Page3[0].Line19_DOB[0]".to_string();
    values.insert(dob.clone(), json!("13/45/1991"));
    let r = c
        .req(M::POST, &format!("/v1/tasks/{id}/submit"), "ana")
        .json(&json!({"mode": "review", "values": values}))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.json::<Value>().await?["fields"][&dob].is_string());

    // Draft round-trip, then submit: flagged resolved from truth, one existing value changed.
    values.insert(dob.clone(), json!(truth[&dob]));
    let city = "form1[0].Page2[0].Pt2Line5_CityOrTown[0]".to_string();
    values.insert(city.clone(), json!("Portland Heights"));
    let draft: Value = c
        .req(M::PUT, &format!("/v1/tasks/{id}/draft"), "ana")
        .json(&json!({"mode": "review", "values": values}))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(draft["updated_by"], "ana");
    let done: Value = c
        .req(M::POST, &format!("/v1/tasks/{id}/submit"), "ana")
        .json(&json!({"mode": "review", "values": values}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(done["task"]["status"], "completed");
    assert!(done["stats"]["changed"].as_u64().unwrap() >= 1);

    let receipt: Value = c
        .http
        .get(format!(
            "{}/v1/artifacts/{}/content",
            storage.url(),
            done["receipt_artifact_id"].as_str().unwrap()
        ))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(receipt["schema"], "receipting.form.v1");
    assert_eq!(receipt["receipted_by"], "ana");
    for (k, v) in &truth {
        let want = if k == &city {
            "Portland Heights"
        } else {
            v.as_str()
        };
        let tf = t.field(k).unwrap();
        let want =
            shepherd_forms::value::normalize(tf.value_type, want, tf.max_len, &tf.options).unwrap();
        let got = receipt["values"][k].as_str().unwrap_or_default();
        assert!(
            got.eq_ignore_ascii_case(&want),
            "{k}: receipt {got:?} != {want:?}"
        );
    }
    let city_field = receipt["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == city)
        .unwrap();
    assert_eq!(
        (
            city_field["provenance"].as_str(),
            city_field["changed"].as_bool()
        ),
        (Some("human"), Some(true))
    );

    let js = async_nats::jetstream::new(async_nats::connect(&infra.nats_url).await?);
    let ev = js
        .get_stream(EVENTS_STREAM)
        .await?
        .get_last_raw_message_by_subject(&subject(RECEIPTING_COMPLETED, doc.parse()?))
        .await?;
    assert_eq!(
        serde_json::from_slice::<Value>(&ev.payload)?["data"]["mode"],
        "review"
    );

    // Full manual receipting of the same document: every field required, all human.
    let manual: Value = c
        .req(M::POST, "/v1/tasks", "ben")
        .json(&json!({"document_id": doc}))
        .send()
        .await?
        .json()
        .await?;
    let mid = manual["id"].as_str().unwrap().to_owned();
    assert_eq!(
        (manual["kind"].as_str(), manual["form_type"].as_str()),
        (Some("manual"), Some("uscis.i-765"))
    );
    let r = c
        .req(M::POST, "/v1/tasks", "ben")
        .json(&json!({"document_id": doc}))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let partial =
        json!({"mode": "full", "values": {"form1[0].Page1[0].Line1a_FamilyName[0]": "Nguyen"}});
    let r = c
        .req(M::POST, &format!("/v1/tasks/{mid}/submit"), "ben")
        .json(&partial)
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let all: BTreeMap<String, Value> = t
        .fields
        .iter()
        .map(|f| (f.id.clone(), json!(truth.get(&f.id))))
        .collect();
    let done: Value = c
        .req(M::POST, &format!("/v1/tasks/{mid}/submit"), "ben")
        .json(&json!({"mode": "full", "values": all}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(done["stats"]["machine"], 0);
    assert_eq!(
        done["stats"]["filled"].as_u64().unwrap() as usize,
        truth.len()
    );

    // A non-form PDF gets an unrecognized task; the reviewer picks the type and receipts fully.
    let up: Value = c
        .http
        .post(format!("{}/v1/intake?filename=letter.pdf", intake.url()))
        .header(header::CONTENT_TYPE, "application/pdf")
        .body(include_bytes!("../../fixtures/multipage.pdf").to_vec())
        .send()
        .await?
        .json()
        .await?;
    let other = c.task_for(up["document_id"].as_str().unwrap()).await?;
    assert!(other["form_type"].is_null());
    let oid = other["id"].as_str().unwrap();
    let r = c
        .req(M::POST, &format!("/v1/tasks/{oid}/submit"), "ana")
        .json(&json!({"mode": "full", "values": {}}))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    c.req(M::PATCH, &format!("/v1/tasks/{oid}"), "ana")
        .json(&json!({"form_type": "uscis.g-1145"}))
        .send()
        .await?
        .error_for_status()?;
    let g: Vec<String> = set
        .get("uscis.g-1145")
        .unwrap()
        .fields
        .iter()
        .map(|f| f.id.clone())
        .collect();
    let vals: BTreeMap<&String, Option<&str>> = g.iter().map(|id| (id, None)).collect();
    let r = c
        .req(M::POST, &format!("/v1/tasks/{oid}/submit"), "ana")
        .json(&json!({"mode": "full", "values": vals}))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::OK);

    let hist: Vec<Value> = c
        .http
        .get(format!("{}/v1/tasks/{id}/history", adj.url()))
        .send()
        .await?
        .json()
        .await?;
    let actions: Vec<&str> = hist.iter().map(|h| h["action"].as_str().unwrap()).collect();
    assert_eq!(actions, ["created", "claimed", "submitted"]);

    adj.stop().await?;
    intake.stop().await?;
    storage.stop().await
}

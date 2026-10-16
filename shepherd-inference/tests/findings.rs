//! Findings end to end: case projections stored in storage → storage events → rule evaluation
//! across cases → findings, events and `inference.findings.v1`. Postgres, NATS and storage are
//! real; projections are written directly instead of by workbench.

use std::{path::PathBuf, sync::Arc, time::Duration};

use futures::StreamExt;
use object_store::{ObjectStore, memory::InMemory};
use reqwest::header;
use serde_json::{Value, json};
use shepherd_common::{
    events::{Envelope, FINDING_RAISED, FINDING_RESOLVED},
    storage_client::StorageClient,
};
use shepherd_storage::testing::{Infra, spawn_storage, wait_ready};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

async fn until<T, F, Fut>(what: &str, f: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Option<T>>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f().await? {
            return Ok(v);
        }
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

struct Case {
    id: Uuid,
    doc: Uuid,
    receipt: &'static str,
}

fn projection(c: &Case, revision: i32, family: &str, given: &str, a_number: &str) -> Value {
    json!({
        "schema": "workbench.case.v1", "case_id": c.id, "receipt_number": c.receipt,
        "form_type": "uscis.i-765", "form_number": "I-765", "benefit": "Employment Authorization",
        "status": "open", "received_at": "2026-09-01", "response_due": null,
        "applicant": { "family_name": family, "given_name": given, "a_number": a_number, "date_of_birth": "04/12/1990" },
        "beneficiary": null, "documents": [{ "document_id": c.doc, "role": "primary" }],
        "decision": null, "revision": revision,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn findings_end_to_end() -> anyhow::Result<()> {
    let infra = Infra::start().await?;
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let storage = spawn_storage(infra.storage_config(), store).await?;
    let sc = StorageClient::new(&storage.url());
    let http = reqwest::Client::new();

    let upload = || async {
        let up: Value = http
            .post(format!("{}/v1/documents?filename=i765.pdf", storage.url()))
            .header(header::CONTENT_TYPE, "application/pdf")
            .body(std::fs::read(root().join("fixtures/sample.pdf"))?)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        anyhow::Ok(up["document_id"].as_str().unwrap().parse::<Uuid>()?)
    };
    let store_projection = |c: &Case, body: Value| {
        let sc = sc.clone();
        let doc = c.doc;
        async move {
            let parent = sc.document(doc).await?.source().unwrap().id;
            sc.create_artifact(
                doc,
                "workbench.case.v1",
                "test",
                parent,
                serde_json::to_vec(&body)?,
            )
            .await?;
            anyhow::Ok(())
        }
    };

    let cfg = shepherd_inference::Config {
        bind_addr: "127.0.0.1:0".into(),
        database_url: infra.url("inference_rw", "inference"),
        nats_url: infra.nats_url.clone(),
        storage_url: storage.url(),
        rules_path: root()
            .join("shepherd-inference/config/rules.toml")
            .to_string_lossy()
            .into_owned(),
        sync_secs: 1,
        db_max_connections: 10,
        consumer_concurrency: 4,
        write_concurrency: 2,
        a_number_case_limit: 50,
    };
    let listener = TcpListener::bind(&cfg.bind_addr).await?;
    let addr = listener.local_addr()?;
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(shepherd_inference::run(cfg, listener, shutdown.clone()));
    wait_ready(addr, "/readyz").await?;
    let base = format!("http://{addr}");

    let nats = async_nats::connect(&infra.nats_url).await?;
    let mut raised = nats.subscribe(format!("{FINDING_RAISED}.>")).await?;
    let mut resolved = nats.subscribe(format!("{FINDING_RESOLVED}.>")).await?;

    let findings = |query: String| {
        let (http, base) = (http.clone(), base.clone());
        async move {
            let r: Value = http
                .get(format!("{base}/v1/findings?{query}"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            anyhow::Ok(r["findings"].as_array().cloned().unwrap_or_default())
        }
    };

    let a = Case {
        id: Uuid::now_v7(),
        doc: upload().await?,
        receipt: "SHP2600000001",
    };
    let b = Case {
        id: Uuid::now_v7(),
        doc: upload().await?,
        receipt: "SHP2600000002",
    };
    let c = Case {
        id: Uuid::now_v7(),
        doc: upload().await?,
        receipt: "SHP2600000003",
    };

    // A and C are the same person (C misspelled); no finding.
    store_projection(&a, projection(&a, 1, "Nguyen", "Linh", "012345678")).await?;
    store_projection(&c, projection(&c, 1, "Nguyen", "Lihn", "012345678")).await?;
    // B claims the same A-Number under another name: both A and B get a finding.
    store_projection(&b, projection(&b, 1, "Okafor", "Chinedu", "012345678")).await?;
    let fb = until("finding on b", || async {
        Ok(findings(format!("case_id={}", b.id))
            .await?
            .into_iter()
            .next())
    })
    .await?;
    assert_eq!(fb["rule"], "a_number_name_mismatch");
    assert_eq!(fb["severity"], "high");
    assert_eq!(fb["key"], "012345678");
    let mut related: Vec<String> = fb["related_cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    related.sort();
    let mut expected = vec![a.id.to_string(), c.id.to_string()];
    expected.sort();
    assert_eq!(related, expected);
    let fa = until("finding on a", || async {
        Ok(findings(format!("case_id={}", a.id))
            .await?
            .into_iter()
            .next())
    })
    .await?;
    assert_eq!(fa["related_cases"], json!([b.id]));
    assert_eq!(fa["evidence"]["consistent"][0]["case_id"], json!(c.id));
    assert!(findings(format!("case_id={}", c.id)).await?.len() == 1);

    let ev: Envelope<Value> = serde_json::from_slice(
        &tokio::time::timeout(Duration::from_secs(10), raised.next())
            .await?
            .unwrap()
            .payload,
    )?;
    assert_eq!(ev.data["rule"], "a_number_name_mismatch");
    assert!(ev.data.get("summary").is_none());

    // Findings artifact on B's primary document.
    let art = until("findings artifact on b", || async {
        Ok(sc
            .document(b.doc)
            .await?
            .latest("inference.findings.v1")
            .cloned())
    })
    .await?;
    let body: Value = serde_json::from_slice(&sc.content(art.id).await?)?;
    assert_eq!(body["findings"][0]["id"], fb["id"]);

    // Dismissing requires a note and hides the finding from the artifact.
    let fid = fa["id"].as_str().unwrap();
    let r = http
        .post(format!("{base}/v1/findings/{fid}/review"))
        .header("x-user", "ana")
        .json(&json!({ "disposition": "dismissed" }))
        .send()
        .await?;
    assert_eq!(r.status(), 400);
    let r: Value = http
        .post(format!("{base}/v1/findings/{fid}/review"))
        .header("x-user", "ana")
        .json(&json!({ "disposition": "dismissed", "note": "B's A-Number was misread" }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(r["review"], "dismissed");
    until("dismissed finding left out of a's artifact", || async {
        let Some(art) = sc
            .document(a.doc)
            .await?
            .latest("inference.findings.v1")
            .cloned()
        else {
            return Ok(None);
        };
        let body: Value = serde_json::from_slice(&sc.content(art.id).await?)?;
        Ok((body["dismissed"] == 1 && body["findings"] == json!([])).then_some(()))
    })
    .await?;

    // A stale projection is ignored.
    store_projection(&b, projection(&b, 0, "Nguyen", "Linh", "012345678")).await?;

    // B's A-Number is corrected: every finding resolves.
    store_projection(&b, projection(&b, 2, "Okafor", "Chinedu", "098765432")).await?;
    until("findings resolved", || async {
        Ok(findings("status=open".into())
            .await?
            .is_empty()
            .then_some(()))
    })
    .await?;
    let ev: Envelope<Value> = serde_json::from_slice(
        &tokio::time::timeout(Duration::from_secs(10), resolved.next())
            .await?
            .unwrap()
            .payload,
    )?;
    assert_eq!(ev.data["rule"], "a_number_name_mismatch");
    let all = findings("status=all".into()).await?;
    assert_eq!(all.len(), 3);
    assert!(all.iter().all(|f| f["status"] == "resolved"));

    let r = http
        .post(format!("{base}/v1/findings/{fid}/review"))
        .header("x-user", "ana")
        .json(&json!({ "disposition": "confirmed" }))
        .send()
        .await?;
    assert_eq!(r.status(), 409);

    let rules: Value = http
        .get(format!("{base}/v1/rules"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(rules["rules"][0]["id"], "a_number_name_mismatch");

    shutdown.cancel();
    handle.await??;
    storage.stop().await?;
    Ok(())
}

/// An A-Number held by more cases than the limit is reported once per case and not compared.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn a_number_over_case_limit() -> anyhow::Result<()> {
    let infra = Infra::start().await?;
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let storage = spawn_storage(infra.storage_config(), store).await?;
    let sc = StorageClient::new(&storage.url());
    let http = reqwest::Client::new();
    let cfg = shepherd_inference::Config {
        bind_addr: "127.0.0.1:0".into(),
        database_url: infra.url("inference_rw", "inference"),
        nats_url: infra.nats_url.clone(),
        storage_url: storage.url(),
        rules_path: root()
            .join("shepherd-inference/config/rules.toml")
            .to_string_lossy()
            .into_owned(),
        sync_secs: 1,
        db_max_connections: 10,
        consumer_concurrency: 4,
        write_concurrency: 2,
        a_number_case_limit: 2,
    };
    let listener = TcpListener::bind(&cfg.bind_addr).await?;
    let addr = listener.local_addr()?;
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(shepherd_inference::run(cfg, listener, shutdown.clone()));
    wait_ready(addr, "/readyz").await?;
    let base = format!("http://{addr}");
    let open = |rule: &'static str| {
        let (http, base) = (http.clone(), base.clone());
        async move {
            let r: Value = http
                .get(format!("{base}/v1/findings?rule={rule}"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            anyhow::Ok(r["findings"].as_array().cloned().unwrap_or_default())
        }
    };

    let mut cases = Vec::new();
    for (receipt, family, given) in [
        ("SHP2600000011", "Nguyen", "Linh"),
        ("SHP2600000012", "Okafor", "Chinedu"),
        ("SHP2600000013", "Mensah", "Ama"),
    ] {
        let up: Value = http
            .post(format!("{}/v1/documents?filename=i765.pdf", storage.url()))
            .header(header::CONTENT_TYPE, "application/pdf")
            .body(std::fs::read(root().join("fixtures/sample.pdf"))?)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let c = Case {
            id: Uuid::now_v7(),
            doc: up["document_id"].as_str().unwrap().parse()?,
            receipt,
        };
        let parent = sc.document(c.doc).await?.source().unwrap().id;
        let body = projection(&c, 1, family, given, "000000000");
        sc.create_artifact(
            c.doc,
            "workbench.case.v1",
            "test",
            parent,
            serde_json::to_vec(&body)?,
        )
        .await?;
        cases.push(c);
    }

    // Two holders: a mismatch. The third takes it over the limit: every case reports the shared
    // A-Number instead, and the mismatches resolve.
    until("widely shared on every case", || async {
        let shared = open("a_number_widely_shared").await?;
        let mismatch = open("a_number_name_mismatch").await?;
        Ok((shared.len() == 3 && mismatch.is_empty()).then_some(shared))
    })
    .await
    .map(|shared| {
        assert!(shared.iter().all(|f| f["related_cases"] == json!([])));
        assert!(shared.iter().all(|f| f["evidence"]["case_limit"] == 2));
    })?;

    // Back under the limit: the remaining holders are compared again.
    let c = &cases[2];
    let parent = sc.document(c.doc).await?.source().unwrap().id;
    let body = projection(c, 2, "Mensah", "Ama", "055555555");
    sc.create_artifact(
        c.doc,
        "workbench.case.v1",
        "test",
        parent,
        serde_json::to_vec(&body)?,
    )
    .await?;
    until("mismatch between the remaining holders", || async {
        let shared = open("a_number_widely_shared").await?;
        let mismatch = open("a_number_name_mismatch").await?;
        Ok((shared.is_empty() && mismatch.len() == 2).then_some(()))
    })
    .await?;

    shutdown.cancel();
    handle.await??;
    storage.stop().await?;
    Ok(())
}

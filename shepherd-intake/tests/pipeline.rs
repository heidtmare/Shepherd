//! Upload → storage → JetStream → extraction → IR artifact, against real Postgres + NATS.
//! Requires Docker: `cargo test -- --ignored`.

use std::{sync::Arc, time::Duration};

use object_store::memory::InMemory;
use reqwest::{StatusCode, header};
use serde_json::{Value, json};
use shepherd_common::events::{
    DLQ_STREAM, DOCUMENT_CREATED, DocumentCreated, EVENTS_STREAM, EXTRACTION_COMPLETED,
    EXTRACTION_FAILED, EXTRACTION_STARTED, Envelope, subject,
};
use shepherd_intake::{Config, PRODUCER};
use shepherd_storage::testing::{Infra, spawn_storage, wait_ready};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MULTIPAGE: &[u8] = include_bytes!("../../fixtures/multipage.pdf");

async fn ir_versions(
    http: &reqwest::Client,
    storage: &str,
    doc: Uuid,
) -> anyhow::Result<Vec<Value>> {
    let d: Value = http
        .get(format!("{storage}/v1/documents/{doc}"))
        .send()
        .await?
        .json()
        .await?;
    Ok(d["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["schema"] == "intake.ir.v1")
        .cloned()
        .collect())
}

async fn wait_versions(
    http: &reqwest::Client,
    storage: &str,
    doc: Uuid,
    n: usize,
) -> anyhow::Result<Vec<Value>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let v = ir_versions(http, storage, doc).await?;
        if v.len() >= n {
            return Ok(v);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "expected {n} IR versions, have {}",
            v.len()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn last_event(
    js: &async_nats::jetstream::Context,
    kind: &str,
    doc: Uuid,
) -> anyhow::Result<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let stream = js.get_stream(EVENTS_STREAM).await?;
    loop {
        if let Ok(m) = stream
            .get_last_raw_message_by_subject(&subject(kind, doc))
            .await
        {
            return Ok(serde_json::from_slice(&m.payload)?);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "no {kind} event for {doc}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn upload_extract_reprocess() -> anyhow::Result<()> {
    let infra = Infra::start().await?;
    let storage = spawn_storage(infra.storage_config(), Arc::new(InMemory::new())).await?;
    let cfg = Config {
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
        recognition_enabled: false,
        forms_dir: String::new(),
        pdfium_lib_dir: String::new(),
        htr_model_dir: String::new(),
        recognition_concurrency: 1,
        recognition_timeout_secs: 60,
        recognition_blank_cache_mb: 64,
    };
    let listener = TcpListener::bind(&cfg.bind_addr).await?;
    let addr = listener.local_addr()?;
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(shepherd_intake::run(cfg, listener, shutdown.clone()));
    wait_ready(addr, "/readyz").await?;
    let base = format!("http://{addr}");
    let http = reqwest::Client::new();
    let js = async_nats::jetstream::new(async_nats::connect(&infra.nats_url).await?);

    // Form and content-type validation.
    let page = http.get(&base).send().await?.text().await?;
    assert!(page.contains("Shepherd intake"));
    let r = http
        .post(format!("{base}/v1/intake"))
        .header(header::CONTENT_TYPE, "text/plain")
        .body("x")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // Upload → IR artifact.
    let accepted: Value = http
        .post(format!("{base}/v1/intake?filename=multipage.pdf"))
        .header(header::CONTENT_TYPE, "application/pdf")
        .body(MULTIPAGE)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let doc: Uuid = accepted["document_id"].as_str().unwrap().parse()?;
    let source: Uuid = accepted["artifact_id"].as_str().unwrap().parse()?;
    assert_eq!(
        accepted["events_url"],
        format!("http://stream.invalid/v1/events?document_id={doc}")
    );

    let v = wait_versions(&http, &storage.url(), doc, 1).await?;
    assert_eq!(v[0]["parent_artifact_id"], source.to_string());
    assert_eq!(v[0]["produced_by"], PRODUCER);
    let ir: Value = http
        .get(format!(
            "{}/v1/artifacts/{}/content",
            storage.url(),
            v[0]["id"].as_str().unwrap()
        ))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(
        ir["stats"],
        json!({"pages": 3, "text_pages": 2, "empty_pages": 1, "chars": ir["stats"]["chars"]})
    );
    assert_eq!(ir["flags"]["needs_ocr"], true);
    assert_eq!(ir["title"], "Property damage claim");
    assert_eq!(ir["source"]["filename"], "multipage.pdf");
    assert!(
        ir["pages"][1]["text"]
            .as_str()
            .unwrap()
            .contains("mold remediation")
    );

    let started = last_event(&js, EXTRACTION_STARTED, doc).await?;
    assert_eq!(started["data"]["source_artifact_id"], source.to_string());
    let completed = last_event(&js, EXTRACTION_COMPLETED, doc).await?;
    assert_eq!(completed["data"]["version"], 1);
    assert_eq!(completed["data"]["text_pages"], 2);

    // A redelivered document.created does not produce a second version.
    let dup = Envelope::new(
        DOCUMENT_CREATED,
        "test",
        doc,
        DocumentCreated {
            document_id: doc,
            artifact_id: source,
            sha256: String::new(),
            content_type: "application/pdf".into(),
            size_bytes: 0,
            filename: None,
        },
    );
    js.publish(dup.nats_subject(), serde_json::to_vec(&dup)?.into())
        .await?
        .await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(ir_versions(&http, &storage.url(), doc).await?.len(), 1);

    // Manual reprocess creates version 2.
    let r = http
        .post(format!("{base}/v1/intake/{doc}/reprocess"))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let v = wait_versions(&http, &storage.url(), doc, 2).await?;
    assert!(v.iter().any(|a| a["version"] == 2));
    let r = http
        .post(format!("{base}/v1/intake/{}/reprocess", Uuid::now_v7()))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    // Unparseable PDF: terminal failure event and a DLQ entry, no retries.
    let bad: Value = http
        .post(format!("{base}/v1/intake"))
        .header(header::CONTENT_TYPE, "application/pdf")
        .body("%PDF-1.7 this is not a pdf")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let bad_id: Uuid = bad["document_id"].as_str().unwrap().parse()?;
    let failed = last_event(&js, EXTRACTION_FAILED, bad_id).await?;
    assert_eq!(failed["data"]["terminal"], true);
    assert_eq!(failed["data"]["attempt"], 1);
    let mut dlq = js.get_stream(DLQ_STREAM).await?;
    assert_eq!(dlq.info().await?.state.messages, 1);
    assert!(ir_versions(&http, &storage.url(), bad_id).await?.is_empty());

    shutdown.cancel();
    handle.await??;
    storage.stop().await
}

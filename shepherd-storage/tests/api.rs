//! Storage API against real Postgres + NATS. Requires Docker: `cargo test -- --ignored`.

use std::{sync::Arc, time::Duration};

use futures::StreamExt;
use object_store::memory::InMemory;
use reqwest::{StatusCode, header};
use serde_json::Value;
use shepherd_storage::testing::{Infra, spawn_storage};

const PDF: &[u8] = include_bytes!("../../fixtures/sample.pdf");
const IR: &str = include_str!("../../fixtures/sample.ir.json");

#[tokio::test]
#[ignore = "requires Docker"]
async fn document_lifecycle() -> anyhow::Result<()> {
    let infra = Infra::start().await?;
    let svc = spawn_storage(infra.storage_config(), Arc::new(InMemory::new())).await?;
    let base = svc.url();
    let http = reqwest::Client::new();

    // Upload validation.
    let r = http
        .post(format!("{base}/v1/documents"))
        .header(header::CONTENT_TYPE, "text/plain")
        .body(PDF)
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let r = http
        .post(format!("{base}/v1/documents"))
        .header(header::CONTENT_TYPE, "application/pdf")
        .body("<html>")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // Upload.
    let r = http
        .post(format!("{base}/v1/documents?filename=claim-001.pdf"))
        .header(header::CONTENT_TYPE, "application/pdf")
        .header("x-actor", "tester")
        .body(PDF)
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::CREATED);
    let created: Value = r.json().await?;
    let doc = created["document_id"].as_str().unwrap().to_owned();
    let sha = created["sha256"].as_str().unwrap().to_owned();

    // Source download, range, conditional.
    let r = http
        .get(format!("{base}/v1/documents/{doc}/source"))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()[header::ETAG], format!("\"{sha}\""));
    assert_eq!(&r.bytes().await?[..], PDF);
    let r = http
        .get(format!("{base}/v1/documents/{doc}/source"))
        .header(header::RANGE, "bytes=0-4")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        r.headers()[header::CONTENT_RANGE],
        format!("bytes 0-4/{}", PDF.len())
    );
    assert_eq!(&r.bytes().await?[..], b"%PDF-");
    let r = http
        .get(format!("{base}/v1/documents/{doc}/source"))
        .header(header::RANGE, format!("bytes={}-", PDF.len()))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    let r = http
        .get(format!("{base}/v1/documents/{doc}/source"))
        .header(header::IF_NONE_MATCH, format!("\"{sha}\""))
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::NOT_MODIFIED);

    // Derived artifacts: versioning and validation.
    let post_ir = |q: String, body: String| {
        http.post(format!("{base}/v1/documents/{doc}/artifacts?{q}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
    };
    let v1: Value = post_ir("schema=intake.ir.v1&produced_by=test".into(), IR.into())
        .await?
        .json()
        .await?;
    assert_eq!(v1["version"], 1);
    let r = post_ir(
        format!(
            "schema=intake.ir.v1&parent_artifact_id={}",
            v1["id"].as_str().unwrap()
        ),
        IR.into(),
    )
    .await?;
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(r.json::<Value>().await?["version"], 2);
    assert_eq!(
        post_ir("schema=intake.ir.v1".into(), "{not json".into())
            .await?
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post_ir("schema=Bad/Schema".into(), "{}".into())
            .await?
            .status(),
        StatusCode::BAD_REQUEST
    );
    let r = post_ir(
        format!("schema=x&parent_artifact_id={}", uuid::Uuid::now_v7()),
        "{}".into(),
    )
    .await?;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = http
        .post(format!(
            "{base}/v1/documents/{}/artifacts?schema=x",
            uuid::Uuid::now_v7()
        ))
        .body("{}")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    let detail: Value = http
        .get(format!("{base}/v1/documents/{doc}"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(detail["artifacts"].as_array().unwrap().len(), 3);
    let content: Value = http
        .get(format!(
            "{base}/v1/artifacts/{}/content",
            v1["id"].as_str().unwrap()
        ))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(content, serde_json::from_str::<Value>(IR)?);

    let audit: Vec<Value> = http
        .get(format!("{base}/v1/documents/{doc}/audit"))
        .send()
        .await?
        .json()
        .await?;
    let actions: Vec<_> = audit
        .iter()
        .map(|a| a["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        actions,
        ["document.created", "artifact.created", "artifact.created"]
    );
    assert_eq!(audit[0]["actor"], "tester");

    let page: Value = http
        .get(format!("{base}/v1/documents?limit=10"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);

    // Append-only enforcement.
    let su = sqlx::PgPool::connect(&infra.superuser_url()).await?;
    let err = sqlx::query("UPDATE storage.artifacts SET produced_by = 'x'")
        .execute(&su)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("append-only"));

    // Outbox relayed to JetStream: 1 document + 2 artifact events.
    let nc = async_nats::connect(&infra.nats_url).await?;
    let js = async_nats::jetstream::new(nc);
    let stream = js
        .get_stream(shepherd_common::events::EVENTS_STREAM)
        .await?;
    let consumer = stream
        .create_consumer(async_nats::jetstream::consumer::pull::Config {
            filter_subject: "shepherd.storage.>".into(),
            ..Default::default()
        })
        .await?;
    let mut msgs = consumer
        .fetch()
        .max_messages(10)
        .expires(Duration::from_secs(2))
        .messages()
        .await?;
    let mut subjects = Vec::new();
    while let Some(m) = msgs.next().await {
        subjects.push(m.map_err(|e| anyhow::anyhow!(e))?.subject.to_string());
    }
    use shepherd_common::events::{ARTIFACT_CREATED, DOCUMENT_CREATED, subject};
    let doc_id: uuid::Uuid = doc.parse()?;
    assert_eq!(
        subjects,
        [
            subject(DOCUMENT_CREATED, doc_id),
            subject(ARTIFACT_CREATED, doc_id),
            subject(ARTIFACT_CREATED, doc_id)
        ]
    );
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM storage.outbox WHERE published_at IS NULL")
            .fetch_one(&su)
            .await?;
    assert_eq!(pending, 0);

    // Prune removes only published rows past retention.
    sqlx::query(
        "UPDATE storage.outbox SET published_at = now() - interval '8 days'
         WHERE id = (SELECT min(id) FROM storage.outbox)",
    )
    .execute(&su)
    .await?;
    assert_eq!(shepherd_storage::outbox::prune(&su, 7).await?, 1);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM storage.outbox")
        .fetch_one(&su)
        .await?;
    assert_eq!(left, 2);

    // Derived PDF (e.g. a generated letter): stored and served with its media type.
    let post_pdf = |body: Vec<u8>| {
        http.post(format!(
            "{base}/v1/documents/{doc}/artifacts?schema=notification.letter.pdf"
        ))
        .header(header::CONTENT_TYPE, "application/pdf")
        .body(body)
        .send()
    };
    let r = post_pdf(PDF.to_vec()).await?;
    assert_eq!(r.status(), StatusCode::CREATED);
    let letter: Value = r.json().await?;
    let r = http
        .get(format!(
            "{base}/v1/artifacts/{}/content",
            letter["id"].as_str().unwrap()
        ))
        .send()
        .await?;
    assert_eq!(r.headers()[header::CONTENT_TYPE], "application/pdf");
    assert_eq!(&r.bytes().await?[..], PDF);
    assert_eq!(
        post_pdf(b"{}".to_vec()).await?.status(),
        StatusCode::BAD_REQUEST
    );
    let r = http
        .post(format!("{base}/v1/documents/{doc}/artifacts?schema=x"))
        .header(header::CONTENT_TYPE, "text/plain")
        .body("hello")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    svc.stop().await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn blob_gc() -> anyhow::Result<()> {
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload, path::Path};
    use shepherd_common::blob::content_key;

    let infra = Infra::start().await?;
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let svc = spawn_storage(infra.storage_config(), store.clone()).await?;
    let base = svc.url();
    let http = reqwest::Client::new();
    let keys = || async {
        let v: Vec<String> = store
            .list(None)
            .map(|m| m.unwrap().location.to_string())
            .collect()
            .await;
        v
    };

    let r = http
        .post(format!("{base}/v1/documents"))
        .header(header::CONTENT_TYPE, "application/pdf")
        .body(PDF)
        .send()
        .await?;
    let doc = r.json::<Value>().await?["document_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Rejected artifact uploads write no blob.
    let before = keys().await;
    let r = http
        .post(format!(
            "{base}/v1/documents/{}/artifacts?schema=intake.ir.v1",
            uuid::Uuid::now_v7()
        ))
        .body(IR)
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let r = http
        .post(format!(
            "{base}/v1/documents/{doc}/artifacts?schema=intake.ir.v1&parent_artifact_id={}",
            uuid::Uuid::now_v7()
        ))
        .body(IR)
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(keys().await, before);

    // An orphan whose content is later uploaded is reused, then kept by GC.
    let reused = br#"{"reused": true}"#;
    let reused_key = content_key(&hex::encode(<sha2::Sha256 as sha2::Digest>::digest(reused)));
    store
        .put(&reused_key, PutPayload::from_static(reused))
        .await?;
    let orphan = content_key(&"ab".repeat(32));
    store.put(&orphan, PutPayload::from_static(b"x")).await?;
    let tmp = Path::from("tmp/abandoned");
    store.put(&tmp, PutPayload::from_static(b"x")).await?;
    let r = http
        .post(format!(
            "{base}/v1/documents/{doc}/artifacts?schema=x.reused.v1"
        ))
        .body(&reused[..])
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::CREATED);

    tokio::time::sleep(Duration::from_millis(10)).await;
    let su = sqlx::PgPool::connect(&infra.superuser_url()).await?;
    let gc = shepherd_storage::gc::Gc {
        db: su,
        blobs: shepherd_storage::blob::BlobStore::new(store.clone()),
        grace: Duration::ZERO,
        interval: Duration::from_secs(3600),
    };
    let stats = gc.sweep().await?.expect("sweep lock");
    assert_eq!(stats.deleted, 2, "{stats:?}");
    let left = keys().await;
    assert!(!left.contains(&orphan.to_string()) && !left.contains(&tmp.to_string()));
    assert!(left.contains(&reused_key.to_string()));
    let r = http
        .get(format!("{base}/v1/documents/{doc}/source"))
        .send()
        .await?;
    assert_eq!(&r.bytes().await?[..], PDF);

    svc.stop().await
}

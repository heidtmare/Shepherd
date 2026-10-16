//! Event feed and DLQ tooling against real NATS. Requires Docker: `cargo test -- --ignored`.

use std::time::Duration;

use async_nats::HeaderMap;
use futures::StreamExt;
use serde_json::{Value, json};
use shepherd_common::{
    consumer::{ERROR_HEADER, ORIGINAL_SUBJECT_HEADER, REPLAYED_FROM_HEADER},
    events::{
        DOCUMENT_CREATED, EVENTS_STREAM, EXTRACTION_REQUESTED, EXTRACTION_STARTED, Envelope,
        subject,
    },
};
use shepherd_storage::testing::{Infra, wait_ready};
use shepherd_stream::Config;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const ADMIN: &str = "stream-admin";

async fn publish(js: &async_nats::jetstream::Context, kind: &str, doc: Uuid) -> anyhow::Result<()> {
    let ev = Envelope::new(kind, "test", doc, json!({ "document_id": doc }));
    js.publish(ev.nats_subject(), serde_json::to_vec(&ev)?.into())
        .await?
        .await?;
    Ok(())
}

/// Reads SSE frames until `n` `data:` events arrive; returns (id, envelope) pairs.
async fn read_events(resp: reqwest::Response, n: usize) -> anyhow::Result<Vec<(u64, Value)>> {
    let mut body = resp.bytes_stream();
    let mut buf = String::new();
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while out.len() < n {
        let chunk = tokio::time::timeout_at(deadline, body.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("eof"))??;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(end) = buf.find("\n\n") {
            let frame: String = buf.drain(..end + 2).collect();
            let (mut id, mut data) = (None, None);
            for line in frame.lines() {
                if let Some(v) = line.strip_prefix("id:") {
                    id = v.trim().parse().ok();
                } else if let Some(v) = line.strip_prefix("data:") {
                    data = Some(serde_json::from_str::<Value>(v.trim())?);
                }
            }
            if let (Some(id), Some(data)) = (id, data) {
                out.push((id, data));
            }
        }
    }
    Ok(out)
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn events_and_dlq() -> anyhow::Result<()> {
    let infra = Infra::start().await?;
    let cfg = Config {
        bind_addr: "127.0.0.1:0".into(),
        nats_url: infra.nats_url.clone(),
        topology_path: concat!(env!("CARGO_MANIFEST_DIR"), "/config/topology.toml").into(),
        admin_token: Some(ADMIN.into()),
        cors_allowed_origins: "http://localhost:8083".into(),
    };
    let listener = TcpListener::bind(&cfg.bind_addr).await?;
    let addr = listener.local_addr()?;
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(shepherd_stream::run(cfg, listener, shutdown.clone()));
    wait_ready(addr, "/readyz").await?;
    let base = format!("http://{addr}");
    let http = reqwest::Client::new();
    let js = async_nats::jetstream::new(async_nats::connect(&infra.nats_url).await?);

    // Per-document feed replays history, then streams live events.
    let (doc, other) = (Uuid::now_v7(), Uuid::now_v7());
    publish(&js, DOCUMENT_CREATED, doc).await?;
    publish(&js, DOCUMENT_CREATED, other).await?;
    let resp = http
        .get(format!("{base}/v1/events?document_id={doc}"))
        .header("origin", "http://localhost:8083")
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(
        resp.headers()["access-control-allow-origin"],
        "http://localhost:8083"
    );
    let reader = tokio::spawn(read_events(resp, 2));
    tokio::time::sleep(Duration::from_millis(300)).await;
    publish(&js, EXTRACTION_STARTED, doc).await?;
    let events = reader.await??;
    let kinds: Vec<_> = events
        .iter()
        .map(|(_, e)| e["type"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds, [DOCUMENT_CREATED, EXTRACTION_STARTED]);
    assert!(events.iter().all(|(_, e)| e["subject"] == doc.to_string()));

    // Last-Event-ID resumes after the given sequence.
    let resp = http
        .get(format!("{base}/v1/events?document_id={doc}"))
        .header("last-event-id", events[0].0.to_string())
        .send()
        .await?;
    let resumed = read_events(resp, 1).await?;
    assert_eq!(resumed[0].0, events[1].0);

    let r = http
        .get(format!("{base}/v1/events?subject=shepherd.dlq.>"))
        .send()
        .await?;
    assert_eq!(r.status(), 400);

    // DLQ list → replay → removed, republished with provenance header.
    let target = subject(EXTRACTION_REQUESTED, doc);
    let mut h = HeaderMap::new();
    h.insert(ERROR_HEADER, "boom");
    h.insert(ORIGINAL_SUBJECT_HEADER, target.as_str());
    js.publish_with_headers("shepherd.dlq.intake-extractor", h, r#"{"x":1}"#.into())
        .await?
        .await?;
    let page: Value = http
        .get(format!("{base}/v1/dlq"))
        .send()
        .await?
        .json()
        .await?;
    let item = &page["items"][0];
    assert_eq!(item["error"], "boom");
    assert_eq!(item["payload"], json!({"x": 1}));
    let seq = item["sequence"].as_u64().unwrap();

    let r = http
        .post(format!("{base}/v1/dlq/{seq}/replay"))
        .send()
        .await?;
    assert_eq!(r.status(), 401);
    let r = http
        .post(format!("{base}/v1/dlq/{seq}/replay"))
        .bearer_auth(ADMIN)
        .send()
        .await?;
    assert_eq!(r.status(), 202);
    let page: Value = http
        .get(format!("{base}/v1/dlq"))
        .send()
        .await?
        .json()
        .await?;
    assert!(page["items"].as_array().unwrap().is_empty());
    let replayed = js
        .get_stream(EVENTS_STREAM)
        .await?
        .get_last_raw_message_by_subject(&target)
        .await?;
    assert_eq!(
        replayed.headers.get(REPLAYED_FROM_HEADER).unwrap().as_str(),
        seq.to_string()
    );
    let r = http
        .post(format!("{base}/v1/dlq/{seq}/replay"))
        .bearer_auth(ADMIN)
        .send()
        .await?;
    assert_eq!(r.status(), 404);

    // Topology reports declared consumers.
    let topo: Value = http
        .get(format!("{base}/v1/topology"))
        .send()
        .await?
        .json()
        .await?;
    let consumers: Vec<_> = topo
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| {
            s["consumers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["name"].as_str().unwrap().to_owned())
        })
        .collect();
    assert!(
        consumers.contains(&"search-indexer".to_owned())
            && consumers.contains(&"intake-extractor".to_owned())
    );

    shutdown.cancel();
    handle.await?
}

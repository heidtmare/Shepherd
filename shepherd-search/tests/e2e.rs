//! storage → JetStream → search, against real Postgres + NATS. Requires Docker: `cargo test -- --ignored`.

use std::{collections::HashSet, sync::Arc, time::Duration};

use object_store::{ObjectStore, memory::InMemory};
use reqwest::header;
use serde_json::{Value, json};
use shepherd_common::events::{CARD_INDEXED, EVENTS_STREAM, subject};
use shepherd_search::config::{Config, ContentSourceKind};
use shepherd_storage::testing::{Infra, Running, memory_blob_config, spawn_storage, wait_ready};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const PDF: &[u8] = include_bytes!("../../fixtures/sample.pdf");
const IR: &str = include_str!("../../fixtures/sample.ir.json");
const CLAIM: &str = include_str!("../../fixtures/sample.claim.json");
const ADMIN: &str = "test-admin";

async fn spawn_search(infra: &Infra, store: Arc<dyn ObjectStore>) -> anyhow::Result<Running> {
    let cfg = Config {
        bind_addr: "127.0.0.1:0".into(),
        database_url: infra.url("search_rw", "search"),
        read_database_url: None,
        nats_url: infra.nats_url.clone(),
        mapping_path: concat!(env!("CARGO_MANIFEST_DIR"), "/config/mapping.toml").into(),
        content_source: ContentSourceKind::Blob,
        storage_url: None,
        blob: memory_blob_config(),
        admin_token: Some(ADMIN.into()),
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
    };
    let listener = TcpListener::bind(&cfg.bind_addr).await?;
    let addr = listener.local_addr()?;
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(shepherd_search::run(
        cfg,
        Some(store),
        listener,
        shutdown.clone(),
    ));
    wait_ready(addr, "/readyz").await?;
    Ok(Running {
        addr,
        shutdown,
        handle,
    })
}

struct Client {
    http: reqwest::Client,
    storage: String,
    search: String,
}

impl Client {
    /// Uploads the sample PDF with an intake IR and the given claim enrichment.
    async fn upload(&self, filename: &str, claim: &Value) -> anyhow::Result<String> {
        let r: Value = self
            .http
            .post(format!("{}/v1/documents?filename={filename}", self.storage))
            .header(header::CONTENT_TYPE, "application/pdf")
            .body(PDF)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let doc = r["document_id"].as_str().unwrap().to_owned();
        self.artifact(&doc, "intake.ir.v1", &serde_json::from_str(IR)?)
            .await?;
        self.artifact(&doc, "inference.claim.v1", claim).await?;
        Ok(doc)
    }

    async fn artifact(&self, doc: &str, schema: &str, body: &Value) -> anyhow::Result<()> {
        self.http
            .post(format!(
                "{}/v1/documents/{doc}/artifacts?schema={schema}",
                self.storage
            ))
            .json(body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn search(&self, body: Value) -> anyhow::Result<Value> {
        Ok(self
            .http
            .post(format!("{}/v1/search", self.search))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn ids(&self, body: Value) -> anyhow::Result<Vec<String>> {
        let r = self.search(body).await?;
        Ok(r["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["doc_id"].as_str().unwrap().to_owned())
            .collect())
    }

    /// Polls until `pred` holds on the search response (indexing is asynchronous).
    async fn eventually(
        &self,
        body: Value,
        pred: impl Fn(&Value) -> bool,
    ) -> anyhow::Result<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let r = self.search(body.clone()).await?;
            if pred(&r) {
                return Ok(r);
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "condition not met: {r}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn storage_to_search() -> anyhow::Result<()> {
    let infra = Infra::start().await?;
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let storage = spawn_storage(infra.storage_config(), store.clone()).await?;
    let search = spawn_search(&infra, store).await?;
    let c = Client {
        http: reqwest::Client::new(),
        storage: storage.url(),
        search: search.url(),
    };

    let claim: Value = serde_json::from_str(CLAIM)?;
    let doc = c.upload("claim-001.pdf", &claim).await?;

    // Full-text hit with highlight, facets and page matches.
    let r = c
        .eventually(
            json!({"q": "mold remediation", "include_pages": true, "facets": ["type", "status"]}),
            // Both artifacts (IR and claim) must be indexed.
            |r| r["hits"][0]["summary"]["amount"].is_string(),
        )
        .await?;
    let hit = &r["hits"][0];
    assert_eq!(hit["doc_id"], doc);
    assert_eq!(hit["matched_pages"], json!([2]));
    assert!(hit["highlight"].as_str().unwrap().contains("<mark>"));
    assert_eq!(
        r["facet_counts"]["type"],
        json!([{"value": "insurance_claim", "count": 1}])
    );
    assert_eq!(r["facet_counts_exact"], true);
    assert_eq!(hit["summary"]["amount"], "18450.75");

    // Fuzzy filename match.
    assert_eq!(
        c.ids(json!({"q": "claim-01.pdf"})).await?,
        std::slice::from_ref(&doc)
    );

    // Filters.
    let f = |filters: Value| json!({"filters": filters});
    assert_eq!(
        c.ids(f(
            json!([{"op": "eq", "field": "status", "value": "pending_review"}])
        ))
        .await?
        .len(),
        1
    );
    assert_eq!(
        c.ids(f(
            json!([{"op": "range", "field": "amount", "gte": 18000, "lt": "19000"}])
        ))
        .await?
        .len(),
        1
    );
    assert!(
        c.ids(f(json!([{"op": "range", "field": "amount", "gt": 20000}])))
            .await?
            .is_empty()
    );
    assert_eq!(
        c.ids(f(
            json!([{"op": "range", "field": "received_at", "gte": "2026-08-01T00:00:00Z"}])
        ))
        .await?
        .len(),
        1
    );
    assert_eq!(
        c.ids(f(
            json!([{"op": "contains", "value": {"inference.claim.v1": {"totals": {"currency": "USD"}}}}])
        ))
        .await?
        .len(),
        1
    );
    assert_eq!(
        c.ids(f(json!([{"op": "facet", "value": "party_role/claimant"}])))
            .await?
            .len(),
        1
    );
    assert_eq!(
        c.ids(f(
            json!([{"op": "in", "field": "schema", "values": ["intake.ir.v1"]}])
        ))
        .await?
        .len(),
        1
    );
    let r = c
        .http
        .post(format!("{}/v1/search", c.search))
        .json(&f(json!([{"op": "eq", "field": "x", "value": 1}])))
        .send()
        .await?;
    assert_eq!(r.status(), 400);

    // New artifact version replaces the card content.
    let mut v2 = claim.clone();
    v2["status"] = json!("approved");
    c.artifact(&doc, "inference.claim.v1", &v2).await?;
    c.eventually(
        f(json!([{"op": "eq", "field": "status", "value": "approved"}])),
        |r| r["hits"][0]["doc_id"] == doc,
    )
    .await?;

    // Keyset pagination over sorted results.
    let mut all = HashSet::from([doc.clone()]);
    for i in 0..6 {
        let mut claim = claim.clone();
        claim["totals"]["amount"] = json!(100 + i);
        all.insert(c.upload(&format!("doc-{i}.pdf"), &claim).await?);
    }
    c.eventually(json!({"limit": 100}), |r| {
        r["hits"].as_array().unwrap().len() == all.len()
    })
    .await?;
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let r = c
            .search(
                json!({"sort": {"field": "amount", "dir": "asc"}, "limit": 3, "cursor": cursor}),
            )
            .await?;
        for h in r["hits"].as_array().unwrap() {
            seen.push((
                h["summary"]["amount"].as_str().unwrap().parse::<f64>()?,
                h["doc_id"].as_str().unwrap().to_owned(),
            ));
        }
        match r["next_cursor"].as_str() {
            Some(n) => cursor = Some(n.to_owned()),
            None => break,
        }
    }
    assert_eq!(seen.len(), all.len());
    assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0));
    assert_eq!(
        seen.iter().map(|s| s.1.clone()).collect::<HashSet<_>>(),
        all
    );

    // Card detail.
    let card: Value = c
        .http
        .get(format!("{}/v1/cards/{doc}", c.search))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(card["fields"]["inference.claim.v1"]["status"], "approved");

    // Applied writes are announced on the event stream.
    let js = async_nats::jetstream::new(async_nats::connect(&infra.nats_url).await?);
    let events = js.get_stream(EVENTS_STREAM).await?;
    let indexed = events
        .get_last_raw_message_by_subject(&subject(CARD_INDEXED, doc.parse()?))
        .await?;
    let ev: Value = serde_json::from_slice(&indexed.payload)?;
    assert_eq!(ev["data"]["document_id"], doc);

    // Rebuild restores a wiped projection; admin auth enforced.
    let su = sqlx::PgPool::connect(&infra.superuser_url()).await?;
    sqlx::query("DELETE FROM search.cards").execute(&su).await?;
    assert!(c.ids(json!({})).await?.is_empty());
    let r = c
        .http
        .post(format!("{}/admin/rebuild", c.search))
        .send()
        .await?;
    assert_eq!(r.status(), 401);
    let r = c
        .http
        .post(format!("{}/admin/rebuild", c.search))
        .bearer_auth(ADMIN)
        .send()
        .await?;
    assert_eq!(r.status(), 202);
    c.eventually(json!({"limit": 100}), |r| {
        r["hits"].as_array().unwrap().len() == all.len()
    })
    .await?;
    let status = || async {
        let v: Value = c
            .http
            .get(format!("{}/admin/rebuild", c.search))
            .bearer_auth(ADMIN)
            .send()
            .await?
            .json()
            .await?;
        anyhow::Ok(v)
    };
    let finished = || async {
        for _ in 0..100 {
            let s = status().await?;
            if s["state"] != "running" {
                return anyhow::Ok(s);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        anyhow::bail!("rebuild did not finish")
    };
    let s = finished().await?;
    assert_eq!(s["state"], "succeeded", "{s}");
    assert_eq!(s["failed"], 0);
    assert_eq!(s["indexed"], all.len());
    assert_eq!(s["shards"], s["shards_done"]);

    // A shard held by a dead worker (stale heartbeat) is reclaimed and the rebuild completes.
    sqlx::query("DELETE FROM search.cards").execute(&su).await?;
    let id: i64 =
        sqlx::query_scalar("INSERT INTO search.rebuilds (state) VALUES ('running') RETURNING id")
            .fetch_one(&su)
            .await?;
    sqlx::query(
        "INSERT INTO search.rebuild_shards (rebuild_id, shard, owner, heartbeat_at)
         VALUES ($1, 0, gen_random_uuid(), now() - interval '1 hour')",
    )
    .bind(id)
    .execute(&su)
    .await?;
    c.eventually(json!({"limit": 100}), |r| {
        r["hits"].as_array().unwrap().len() == all.len()
    })
    .await?;
    let s = finished().await?;
    assert_eq!(s["state"], "succeeded", "{s}");
    assert_eq!(s["indexed"], all.len());

    // Declared columns: added on sync, indexed in the background, type changes rejected.
    let rw = sqlx::PgPool::connect(&infra.url("search_rw", "search")).await?;
    let mut cols: shepherd_search::mapping::Columns =
        toml::from_str("priority = { type = \"numeric\", sort = true }")?;
    shepherd_search::columns::sync(&rw, &cols).await?;
    shepherd_search::columns::sync(&rw, &cols).await?;
    shepherd_search::columns::ensure_indexes(&rw, &cols).await?;
    let valid: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
         WHERE c.relname LIKE 'cards_col_%' AND i.indisvalid ORDER BY 1",
    )
    .fetch_all(&su)
    .await?;
    for name in [
        "cards_col_priority_asc_idx",
        "cards_col_priority_desc_idx",
        "cards_col_priority_idx",
        "cards_col_received_at_desc_idx",
        "cards_col_status_idx",
    ] {
        assert!(valid.contains(&name.to_string()), "{name} in {valid:?}");
    }
    cols = toml::from_str("priority = { type = \"text\" }")?;
    assert!(shepherd_search::columns::sync(&rw, &cols).await.is_err());

    search.stop().await?;
    storage.stop().await
}

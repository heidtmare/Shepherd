//! shepherd-inference: rules evaluated on benefit cases as their data changes.
//!
//! Case projections (`workbench.case.v1`) feed an index of the parties of every case. Rules
//! from `config/rules.toml` are evaluated against a case and the cases it shares identifiers
//! with, and produce findings for review: stored here, published as events, and written to the
//! case's primary document as `inference.findings.v1`.

pub mod api;
pub mod config;
pub mod consumer;
pub mod db;
pub mod engine;
pub mod names;
pub mod rules;

use std::sync::Arc;

use shepherd_common::{storage_client::StorageClient, telemetry::PrometheusHandle};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub use config::Config;

pub const SERVICE: &str = "shepherd-inference";
pub const PRODUCER: &str = concat!("shepherd-inference/", env!("CARGO_PKG_VERSION"));

#[derive(Clone)]
pub struct AppState {
    pub engine: engine::Engine,
    pub nats: async_nats::Client,
    pub metrics: PrometheusHandle,
}

/// Publishes a domain event; failures are logged, not returned (the state change is committed).
pub async fn publish<T: serde::Serialize>(
    js: &async_nats::jetstream::Context,
    kind: &str,
    document_id: uuid::Uuid,
    data: T,
) {
    let ev = shepherd_common::events::Envelope::new(kind, SERVICE, document_id, data);
    if let Err(e) = async {
        let payload = serde_json::to_vec(&ev)?;
        js.publish(ev.nats_subject(), payload.into()).await?.await?;
        anyhow::Ok(())
    }
    .await
    {
        tracing::warn!(error = %e, kind, "event not published");
    }
}

pub async fn run(
    cfg: Config,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let rules = Arc::new(rules::RuleSet::load(&cfg.rules_path)?);
    let db = db::connect(&cfg.database_url, cfg.db_max_connections).await?;
    db::migrate(&db).await?;
    let nats = async_nats::connect(cfg.nats_url.as_str()).await?;
    let engine = engine::Engine::new(
        db,
        StorageClient::new(&cfg.storage_url),
        async_nats::jetstream::new(nats.clone()),
        rules,
        cfg.a_number_case_limit,
        cfg.write_concurrency,
    );

    let writer_task = tokio::spawn(engine.clone().run_writer(
        std::time::Duration::from_secs(cfg.sync_secs.max(1)),
        shutdown.clone(),
    ));
    let reconcile_task = tokio::spawn({
        let (e, sd) = (engine.clone(), shutdown.clone());
        async move { e.reconcile(sd).await }
    });
    let consumer_task = tokio::spawn({
        let (e, sd, n) = (engine.clone(), shutdown.clone(), cfg.consumer_concurrency);
        async move {
            if let Err(err) = consumer::run(e, n, sd.clone()).await {
                tracing::error!(error = format!("{err:#}"), "consumer stopped");
                sd.cancel();
            }
        }
    });

    let state = AppState {
        engine,
        nats,
        metrics: shepherd_common::telemetry::metrics_handle(),
    };
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await?;
    consumer_task.await?;
    reconcile_task.await?;
    writer_task.await?;
    Ok(())
}

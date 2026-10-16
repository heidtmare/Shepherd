//! shepherd-stream: owns the JetStream topology and exposes the event feed and DLQ tooling.

pub mod api;
pub mod config;
pub mod error;
pub mod sse;

use std::{sync::Arc, time::Duration};

use async_nats::jetstream;
use shepherd_common::{telemetry::PrometheusHandle, topology::Topology};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub use config::Config;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub topology: Arc<Topology>,
    pub nats: async_nats::Client,
    pub js: jetstream::Context,
    pub metrics: PrometheusHandle,
}

/// Applies the topology (retrying while JetStream starts), then serves HTTP until `shutdown`.
/// `/healthz` only answers after the topology is in place, so dependents can gate on it.
pub async fn run(
    cfg: Config,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let topology = Topology::parse(&std::fs::read_to_string(&cfg.topology_path)?)?;
    let nats = async_nats::connect(cfg.nats_url.as_str()).await?;
    let js = jetstream::new(nats.clone());

    let mut attempt = 0;
    loop {
        match topology.apply(&js).await {
            Ok(()) => break,
            Err(e) if attempt < 30 => {
                attempt += 1;
                tracing::warn!(
                    error = format!("{e:#}"),
                    attempt,
                    "topology apply failed; retrying"
                );
                tokio::select! {
                    _ = shutdown.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
            Err(e) => return Err(e),
        }
    }
    tracing::info!(
        streams = topology.streams.len(),
        consumers = topology.consumers.len(),
        "topology applied"
    );

    let state = AppState {
        cfg: Arc::new(cfg),
        topology: Arc::new(topology),
        nats,
        js,
        metrics: shepherd_common::telemetry::metrics_handle(),
    };
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await?;
    Ok(())
}

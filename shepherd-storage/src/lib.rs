//! shepherd-storage: persists source PDFs and derived JSON artifacts, with audit trail and
//! outbox-published events.

pub mod api;
pub mod blob;
pub mod config;
pub mod db;
pub mod error;
pub mod gc;
pub mod outbox;
#[cfg(feature = "testing")]
pub mod testing;

use std::{sync::Arc, time::Duration};

use object_store::ObjectStore;
use tokio::{net::TcpListener, sync::Notify};
use tokio_util::sync::CancellationToken;

pub use config::Config;

pub const SERVICE: &str = "shepherd-storage";

/// Runs migrations, the outbox relay (when NATS is configured) and the HTTP server until `shutdown`.
pub async fn run(
    cfg: Config,
    store: Arc<dyn ObjectStore>,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let db = db::connect(&cfg.database_url, cfg.db_max_connections).await?;
    db::migrate(&db).await?;

    let relay_wake = Arc::new(Notify::new());
    let nats = match &cfg.nats_url {
        Some(url) => Some(async_nats::connect(url.as_str()).await?),
        None => {
            tracing::warn!("NATS_URL unset; outbox relay disabled");
            None
        }
    };
    let relay = if let Some(client) = &nats {
        let js = async_nats::jetstream::new(client.clone());
        let relay = outbox::Relay {
            db: db.clone(),
            js,
            wake: relay_wake.clone(),
            batch: cfg.outbox_batch,
            poll: Duration::from_millis(cfg.outbox_poll_ms),
            retention_days: cfg.outbox_retention_days,
        };
        Some(tokio::spawn(relay.run(shutdown.clone())))
    } else {
        None
    };

    let blobs = blob::BlobStore::new(store);
    let gc = (cfg.blob_gc_interval_hours > 0).then(|| {
        tokio::spawn(
            gc::Gc {
                db: db.clone(),
                blobs: blobs.clone(),
                grace: Duration::from_secs(cfg.blob_gc_grace_hours * 3600),
                interval: Duration::from_secs(cfg.blob_gc_interval_hours * 3600),
            }
            .run(shutdown.clone()),
        )
    });

    let state = api::AppState {
        cfg: Arc::new(cfg),
        db,
        blobs,
        relay_wake,
        nats,
        metrics: shepherd_common::telemetry::metrics_handle(),
    };
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await?;
    if let Some(h) = relay {
        h.await?;
    }
    if let Some(h) = gc {
        h.await?;
    }
    Ok(())
}

//! shepherd-notification: outbound correspondence for benefit cases.
//!
//! Receipt notices, requests for evidence, notices of intent to deny and decision notices are
//! rendered to PDF, stored in shepherd-storage, queued for mailing and, for receipt notices with an
//! e-notification request, e-mailed.

pub mod api;
pub mod config;
pub mod consumer;
pub mod db;
pub mod email;
pub mod letters;
pub mod notifier;
pub mod pdf;
pub mod workbench;

use std::sync::Arc;

use shepherd_common::{storage_client::StorageClient, telemetry::PrometheusHandle};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub use config::Config;

pub const SERVICE: &str = "shepherd-notification";
pub const PRODUCER: &str = concat!("shepherd-notification/", env!("CARGO_PKG_VERSION"));

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub nats: async_nats::Client,
    pub notifier: notifier::Notifier,
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
    let letters = Arc::new(letters::LetterConfig::load(&cfg.letters_path)?);
    let mailer = match &cfg.smtp_url {
        Some(url) => Some(email::Mailer::new(url, &cfg.email_from)?),
        None => {
            tracing::warn!("SMTP_URL not set; e-mail deliveries stay queued");
            None
        }
    };
    let db = db::connect(&cfg.database_url, cfg.db_max_connections).await?;
    db::migrate(&db).await?;
    let nats = async_nats::connect(cfg.nats_url.as_str()).await?;
    let js = async_nats::jetstream::new(nats.clone());
    let notifier = notifier::Notifier {
        db: db.clone(),
        storage: StorageClient::new(&cfg.storage_url),
        workbench: workbench::Workbench::new(&cfg.workbench_url),
        js,
        letters,
        mailer,
        email_max_attempts: cfg.email_max_attempts.max(1),
    };

    let retry_task = tokio::spawn(notifier.clone().run(
        std::time::Duration::from_secs(cfg.retry_secs.max(1)),
        shutdown.clone(),
    ));
    let sd = shutdown.clone();
    let n = notifier.clone();
    let consumer_task = tokio::spawn(async move {
        if let Err(e) = consumer::run(n, sd.clone()).await {
            tracing::error!(error = format!("{e:#}"), "consumer stopped");
            sd.cancel();
        }
    });

    let state = AppState {
        db,
        nats,
        notifier,
        metrics: shepherd_common::telemetry::metrics_handle(),
    };
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await?;
    consumer_task.await?;
    retry_task.await?;
    Ok(())
}

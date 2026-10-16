//! shepherd-workbench: receipting of recognized forms and adjudication of benefit cases.

pub mod api;
pub mod benefits;
pub mod case;
pub mod case_api;
pub mod case_db;
pub mod case_ops;
pub mod config;
pub mod consumer;
pub mod correspondence;
pub mod db;
pub mod discovery;
pub mod findings;
pub mod pages;
pub mod projection;
pub mod receipt;
pub mod workload;
pub mod workload_api;
pub mod workload_db;
pub mod workload_ops;

use std::sync::Arc;

use shepherd_common::{storage_client::StorageClient, telemetry::PrometheusHandle};
use shepherd_forms::template::TemplateSet;
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub use config::Config;

pub const SERVICE: &str = "shepherd-workbench";
pub const PRODUCER: &str = concat!("shepherd-workbench/", env!("CARGO_PKG_VERSION"));

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: PgPool,
    pub storage: StorageClient,
    pub nats: async_nats::Client,
    pub js: async_nats::jetstream::Context,
    pub templates: Arc<TemplateSet>,
    pub benefits: Arc<benefits::Benefits>,
    pub projector: projection::Projector,
    pub assigner: workload_ops::Assigner,
    pub pages: Arc<pages::Pages>,
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
    shepherd_forms::render::init(&cfg.pdfium_lib_dir)?;
    let templates = Arc::new(TemplateSet::load(&cfg.forms_dir)?);
    let benefits = Arc::new(benefits::Benefits::load(&cfg.benefits_path, &templates)?);
    anyhow::ensure!(
        case::valid_receipt_prefix(&cfg.receipt_prefix),
        "RECEIPT_PREFIX must be three upper-case letters"
    );
    let db = db::connect(&cfg.database_url, cfg.db_max_connections).await?;
    db::migrate(&db).await?;
    let nats = async_nats::connect(cfg.nats_url.as_str()).await?;
    let js = async_nats::jetstream::new(nats.clone());
    let storage = StorageClient::new(&cfg.storage_url);
    let projector = projection::Projector {
        db: db.clone(),
        storage: storage.clone(),
        benefits: benefits.clone(),
        templates: templates.clone(),
    };
    let sync_task = tokio::spawn(projector.clone().run(
        std::time::Duration::from_secs(cfg.case_sync_secs.max(1)),
        shutdown.clone(),
    ));

    let assigner = workload_ops::Assigner {
        db: db.clone(),
        js: js.clone(),
        projector: projector.clone(),
    };
    let sweep_task = tokio::spawn(assigner.clone().run(
        std::time::Duration::from_secs(cfg.workload_sweep_secs.max(1)),
        shutdown.clone(),
    ));

    let consumer = Arc::new(consumer::Consumer {
        js: js.clone(),
        db: db.clone(),
        storage: storage.clone(),
        open_unrecognized: cfg.open_unrecognized,
        benefits: benefits.clone(),
        templates: templates.clone(),
        projector: projector.clone(),
        assigner: assigner.clone(),
        receipt_prefix: cfg.receipt_prefix.clone(),
    });
    let sd = shutdown.clone();
    let consumer_task = tokio::spawn(async move {
        if let Err(e) = consumer.run(sd.clone()).await {
            tracing::error!(error = format!("{e:#}"), "consumer stopped");
            sd.cancel();
        }
    });

    let sd = shutdown.clone();
    let (cjs, cdb) = (js.clone(), db.clone());
    let correspondence_task = tokio::spawn(async move {
        if let Err(e) = correspondence::run(cjs, cdb, sd.clone()).await {
            tracing::error!(error = format!("{e:#}"), "correspondence consumer stopped");
            sd.cancel();
        }
    });

    let state = AppState {
        pages: Arc::new(pages::Pages::new(cfg.page_dpi)),
        cfg: Arc::new(cfg),
        db,
        storage,
        nats,
        js,
        templates,
        benefits,
        projector,
        assigner,
        metrics: shepherd_common::telemetry::metrics_handle(),
    };
    tracing::info!(addr = %listener.local_addr()?, templates = state.templates.templates.len(), "listening");
    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await?;
    consumer_task.await?;
    correspondence_task.await?;
    sync_task.await?;
    sweep_task.await?;
    Ok(())
}

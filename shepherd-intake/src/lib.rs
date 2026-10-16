//! shepherd-intake: accepts PDFs, extracts their text layer and stores `intake.ir.v1` artifacts.

pub mod api;
pub mod config;
pub mod extract;
pub mod form;
pub mod ir;
pub mod recognizer;
pub mod storage;
pub mod worker;

use std::{sync::Arc, time::Duration};

use shepherd_common::telemetry::PrometheusHandle;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub use config::Config;

pub const SERVICE: &str = "shepherd-intake";
/// Recorded as `produced_by` on every IR artifact.
pub const PRODUCER: &str = concat!("shepherd-intake/", env!("CARGO_PKG_VERSION"));

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub storage: storage::StorageClient,
    pub nats: async_nats::Client,
    pub js: async_nats::jetstream::Context,
    pub metrics: PrometheusHandle,
    /// Template ids accepted as `form_type`; empty when recognition is disabled.
    pub form_types: Arc<Vec<FormType>>,
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct FormType {
    /// Template (edition) id.
    pub id: String,
    /// Form type shared by the template's editions.
    pub form_type: String,
    /// Former template ids.
    pub aliases: Vec<String>,
    pub form_number: String,
    pub title: String,
    pub edition: Option<String>,
}

impl FormType {
    /// `form_type` references are form types, template ids or aliases.
    pub fn known(types: &[FormType], reference: &str) -> bool {
        types.iter().any(|t| {
            t.form_type == reference
                || t.id == reference
                || t.aliases.iter().any(|a| a == reference)
        })
    }
}

fn load_recognizer(
    cfg: &Config,
) -> anyhow::Result<(shepherd_forms::recognize::Recognizer, Option<String>)> {
    use shepherd_forms::{
        htr::Trocr,
        recognize::{Recognizer, RecognizerConfig},
        render,
        template::TemplateSet,
    };
    render::init(&cfg.pdfium_lib_dir)?;
    let set = TemplateSet::load(&cfg.forms_dir)?;
    let (htr, model) = match Trocr::load(&cfg.htr_model_dir) {
        Ok(h) => (
            Some(h),
            std::path::Path::new(&cfg.htr_model_dir)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
        ),
        Err(e) => {
            tracing::error!(
                error = format!("{e:#}"),
                "HTR model not loaded; handwritten fields will all require receipting"
            );
            (None, None)
        }
    };
    Ok((
        Recognizer::new(
            set,
            htr,
            RecognizerConfig {
                blank_cache_bytes: cfg.recognition_blank_cache_mb * 1024 * 1024,
                ..Default::default()
            },
        )?,
        model,
    ))
}

/// Runs the extraction worker and HTTP server until `shutdown`.
pub async fn run(
    cfg: Config,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let nats = async_nats::connect(cfg.nats_url.as_str()).await?;
    let js = async_nats::jetstream::new(nats.clone());
    let storage = storage::StorageClient::new(&cfg.storage_url);

    let worker = Arc::new(worker::Worker {
        js: js.clone(),
        storage: storage.clone(),
        concurrency: cfg.concurrency.max(1),
        extract_timeout: Duration::from_secs(cfg.extract_timeout_secs),
        max_page_chars: cfg.max_page_chars,
    });
    let worker_shutdown = shutdown.clone();
    let worker_task = tokio::spawn(async move {
        if let Err(e) = worker.run(worker_shutdown.clone()).await {
            tracing::error!(error = format!("{e:#}"), "worker stopped");
            worker_shutdown.cancel();
        }
    });

    let mut form_types = Vec::new();
    let recognizer_task = if cfg.recognition_enabled {
        let (rec, model) = tokio::task::block_in_place(|| load_recognizer(&cfg))?;
        form_types = rec
            .set
            .templates
            .iter()
            .map(|t| FormType {
                id: t.id.clone(),
                form_type: t.form_type().to_owned(),
                aliases: t.aliases.clone(),
                form_number: t.form_number.clone(),
                title: t.title.clone(),
                edition: t.edition.clone(),
            })
            .collect();
        let rw = Arc::new(recognizer::RecognizerWorker {
            js: js.clone(),
            storage: storage.clone(),
            recognizer: Arc::new(rec),
            htr_model: model,
            concurrency: cfg.recognition_concurrency.max(1),
            timeout: Duration::from_secs(cfg.recognition_timeout_secs),
            slots: Arc::new(tokio::sync::Semaphore::new(
                cfg.recognition_concurrency.max(1),
            )),
        });
        let sd = shutdown.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = rw.run(sd.clone()).await {
                tracing::error!(error = format!("{e:#}"), "recognizer stopped");
                sd.cancel();
            }
        }))
    } else {
        tracing::warn!("form recognition disabled");
        None
    };

    let state = AppState {
        cfg: Arc::new(cfg),
        storage,
        nats,
        js,
        metrics: shepherd_common::telemetry::metrics_handle(),
        form_types: Arc::new(form_types),
    };
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await?;
    worker_task.await?;
    if let Some(t) = recognizer_task {
        t.await?;
    }
    Ok(())
}

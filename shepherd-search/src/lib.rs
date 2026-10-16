//! shepherd-search: Postgres full-text projection of storage documents, fed by JetStream.

pub mod api;
pub mod artifact_cache;
pub mod backend;
pub mod columns;
pub mod config;
pub mod consumer;
pub mod error;
pub mod facet_cache;
pub mod indexer;
pub mod mapping;
pub mod model;
pub mod query;
pub mod rebuild;

use std::{str::FromStr, sync::Arc};

use object_store::ObjectStore;
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub use config::Config;

pub const SERVICE: &str = "shepherd-search";
use config::ContentSourceKind;

async fn pool(url: &str, max: u32, session: &[(&str, String)]) -> anyhow::Result<PgPool> {
    let mut opts = PgConnectOptions::from_str(url)?;
    if !session.is_empty() {
        opts = opts.options(session.iter().map(|(k, v)| (*k, v.as_str())));
    }
    Ok(PgPoolOptions::new()
        .max_connections(max)
        .connect_with(opts)
        .await?)
}

/// Runs migrations, the JetStream consumer and the HTTP server until `shutdown`.
/// `store` is only used when `content_source = blob`.
pub async fn run(
    cfg: Config,
    store: Option<Arc<dyn ObjectStore>>,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let mapping = mapping::Mapping::load(&cfg.mapping_path)?;

    let write = pool(&cfg.database_url, cfg.db_max_connections, &[]).await?;
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.dangerous_set_table_name("search._sqlx_migrations");
    migrator.run(&write).await?;
    columns::sync(&write, &mapping.columns).await?;
    {
        let (pool, cols) = (write.clone(), mapping.columns.clone());
        tokio::spawn(async move {
            if let Err(e) = columns::ensure_indexes(&pool, &cols).await {
                tracing::error!(error = format!("{e:#}"), "column index build failed");
            }
        });
    }
    let read_url = cfg
        .read_database_url
        .as_deref()
        .unwrap_or(&cfg.database_url);
    let read = pool(
        read_url,
        cfg.db_max_connections,
        &[
            ("statement_timeout", cfg.query_timeout_ms.to_string()),
            ("work_mem", cfg.query_work_mem.clone()),
        ],
    )
    .await?;

    let content = match cfg.content_source {
        ContentSourceKind::Blob => indexer::ContentSource::Blob(match store {
            Some(s) => s,
            None => cfg.blob.build()?,
        }),
        ContentSourceKind::Http => indexer::ContentSource::Http {
            client: reqwest::Client::new(),
            base_url: cfg
                .storage_url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("STORAGE_URL required when CONTENT_SOURCE=http"))?
                .trim_end_matches('/')
                .to_owned(),
        },
    };
    let backend = Arc::new(backend::postgres::PgBackend::new(
        write.clone(),
        read,
        backend::postgres::Limits {
            rank_window: cfg.rank_window,
            facet_window: cfg.facet_window,
            facet_cache_ttl: std::time::Duration::from_millis(cfg.facet_cache_ttl_ms),
        },
        mapping.columns.clone(),
    ));
    let rebuilder = Arc::new(rebuild::Rebuilder::new(
        write.clone(),
        cfg.rebuild_shards,
        cfg.rebuild_concurrency,
    ));
    let indexer = Arc::new(indexer::Indexer {
        db: write,
        content,
        mapping,
        backend,
        cache: artifact_cache::ArtifactCache::new(cfg.artifact_cache_mb * 1024 * 1024),
    });

    let nats = async_nats::connect(cfg.nats_url.as_str()).await?;
    let consumer = consumer::Consumer {
        js: async_nats::jetstream::new(nats.clone()),
        indexer: indexer.clone(),
        batch: cfg.consumer_batch,
        concurrency: cfg.consumer_concurrency,
    };
    let consumer_shutdown = shutdown.clone();
    let consumer_task = tokio::spawn(async move {
        if let Err(e) = consumer.run(consumer_shutdown.clone()).await {
            tracing::error!(error = format!("{e:#}"), "consumer stopped");
            consumer_shutdown.cancel();
        }
    });

    let rebuild_task = tokio::spawn(
        rebuilder
            .clone()
            .run_worker(indexer.clone(), shutdown.clone()),
    );

    let state = api::AppState {
        indexer,
        rebuilder,
        nats,
        admin_token: cfg.admin_token.clone(),
        metrics: shepherd_common::telemetry::metrics_handle(),
    };
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await?;
    consumer_task.await?;
    rebuild_task.await?;
    Ok(())
}

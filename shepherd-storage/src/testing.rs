//! Integration-test harness: Postgres and NATS containers plus in-process services.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use object_store::ObjectStore;
use shepherd_common::blob::{BlobBackend, BlobConfig};
use testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner};
use testcontainers_modules::{
    nats::{Nats, NatsServerCmd},
    postgres::Postgres,
};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::Config;

const INIT_SQL: &str = include_str!("../../docker/postgres/init.sql");
const TOPOLOGY: &str = include_str!("../../shepherd-stream/config/topology.toml");

pub struct Infra {
    pub pg: ContainerAsync<Postgres>,
    pub nats: ContainerAsync<Nats>,
    pg_host: String,
    pg_port: u16,
    pub nats_url: String,
}

impl Infra {
    pub async fn start() -> anyhow::Result<Self> {
        let pg = Postgres::default()
            .with_db_name("shepherd")
            .with_tag("18-alpine")
            .start()
            .await?;
        let nats = Nats::default()
            .with_cmd(&NatsServerCmd::default().with_jetstream())
            .with_tag("2.15-alpine")
            .start()
            .await?;
        let pg_host = pg.get_host().await?.to_string();
        let pg_port = pg.get_host_port_ipv4(5432).await?;
        let nats_url = format!(
            "nats://{}:{}",
            nats.get_host().await?,
            nats.get_host_port_ipv4(4222).await?
        );

        let admin = sqlx::PgPool::connect(&format!(
            "postgres://postgres:postgres@{pg_host}:{pg_port}/shepherd"
        ))
        .await?;
        sqlx::raw_sql(INIT_SQL).execute(&admin).await?;
        admin.close().await;

        let js = async_nats::jetstream::new(async_nats::connect(&nats_url).await?);
        shepherd_common::topology::Topology::parse(TOPOLOGY)?
            .apply(&js)
            .await?;
        Ok(Self {
            pg,
            nats,
            pg_host,
            pg_port,
            nats_url,
        })
    }

    pub fn url(&self, user: &str, password: &str) -> String {
        format!(
            "postgres://{user}:{password}@{}:{}/shepherd",
            self.pg_host, self.pg_port
        )
    }

    pub fn superuser_url(&self) -> String {
        self.url("postgres", "postgres")
    }

    pub fn storage_config(&self) -> Config {
        Config {
            bind_addr: "127.0.0.1:0".into(),
            database_url: self.url("storage_rw", "storage"),
            nats_url: Some(self.nats_url.clone()),
            blob: memory_blob_config(),
            max_pdf_bytes: 16 * 1024 * 1024,
            max_json_bytes: 4 * 1024 * 1024,
            outbox_batch: 64,
            outbox_poll_ms: 100,
            outbox_retention_days: 7,
            db_max_connections: 5,
            blob_gc_interval_hours: 0,
            blob_gc_grace_hours: 24,
        }
    }
}

pub fn memory_blob_config() -> BlobConfig {
    BlobConfig {
        blob_backend: BlobBackend::Memory,
        blob_bucket: "test".into(),
        blob_endpoint: None,
        blob_region: "us-east-1".into(),
        blob_access_key_id: None,
        blob_secret_access_key: None,
        blob_allow_http: false,
        blob_local_path: None,
    }
}

pub struct Running {
    pub addr: SocketAddr,
    pub shutdown: CancellationToken,
    pub handle: JoinHandle<anyhow::Result<()>>,
}

impl Running {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub async fn stop(self) -> anyhow::Result<()> {
        self.shutdown.cancel();
        self.handle.await?
    }
}

/// Starts storage on an ephemeral port and waits for `/readyz`.
pub async fn spawn_storage(cfg: Config, store: Arc<dyn ObjectStore>) -> anyhow::Result<Running> {
    let listener = TcpListener::bind(&cfg.bind_addr).await?;
    let addr = listener.local_addr()?;
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(crate::run(cfg, store, listener, shutdown.clone()));
    wait_ready(addr, "/readyz").await?;
    Ok(Running {
        addr,
        shutdown,
        handle,
    })
}

/// Polls `GET {path}` on `addr` until it returns HTTP 200 (30 s timeout).
pub async fn wait_ready(addr: SocketAddr, path: &str) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(mut s) = tokio::net::TcpStream::connect(addr).await {
            s.write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes())
                .await?;
            let mut status = [0u8; 12];
            if s.read_exact(&mut status).await.is_ok() && &status[9..12] == b"200" {
                return Ok(());
            }
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "{addr}{path} not ready after 30s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

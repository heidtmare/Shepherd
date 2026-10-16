use serde::Deserialize;
use shepherd_common::blob::BlobConfig;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind_addr: String,
    pub database_url: String,
    /// Outbox relay is disabled when unset; events accumulate in `storage.outbox`.
    pub nats_url: Option<String>,
    #[serde(flatten)]
    pub blob: BlobConfig,
    #[serde(default = "default_max_pdf")]
    pub max_pdf_bytes: u64,
    #[serde(default = "default_max_json")]
    pub max_json_bytes: usize,
    #[serde(default = "default_outbox_batch")]
    pub outbox_batch: i64,
    #[serde(default = "default_outbox_poll_ms")]
    pub outbox_poll_ms: u64,
    #[serde(default = "default_outbox_retention_days")]
    pub outbox_retention_days: i32,
    #[serde(default = "default_db_max_connections")]
    pub db_max_connections: u32,
    /// Hours between blob GC sweeps; 0 disables GC.
    #[serde(default = "default_blob_gc_interval_hours")]
    pub blob_gc_interval_hours: u64,
    /// Objects modified more recently than this are never collected.
    #[serde(default = "default_blob_gc_grace_hours")]
    pub blob_gc_grace_hours: u64,
}

fn default_bind() -> String {
    "0.0.0.0:8081".into()
}
fn default_max_pdf() -> u64 {
    512 * 1024 * 1024
}
fn default_max_json() -> usize {
    64 * 1024 * 1024
}
fn default_outbox_batch() -> i64 {
    256
}
fn default_outbox_poll_ms() -> u64 {
    500
}
fn default_outbox_retention_days() -> i32 {
    7
}
fn default_db_max_connections() -> u32 {
    20
}
fn default_blob_gc_interval_hours() -> u64 {
    24
}
fn default_blob_gc_grace_hours() -> u64 {
    24
}

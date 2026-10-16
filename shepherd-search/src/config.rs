use serde::Deserialize;
use shepherd_common::blob::BlobConfig;

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ContentSourceKind {
    /// Read artifact bodies directly from the blob store.
    #[default]
    Blob,
    /// Fetch through shepherd-storage `GET /v1/artifacts/{id}/content`.
    Http,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind_addr: String,
    /// Read/write pool: search schema writes and storage schema reads.
    pub database_url: String,
    /// Query pool; point at a replica to isolate query load. Defaults to `database_url`.
    pub read_database_url: Option<String>,
    pub nats_url: String,
    #[serde(default = "default_mapping_path")]
    pub mapping_path: String,
    #[serde(default)]
    pub content_source: ContentSourceKind,
    pub storage_url: Option<String>,
    #[serde(flatten)]
    pub blob: BlobConfig,
    /// Bearer token for `/admin/*`. Admin routes are disabled when unset.
    pub admin_token: Option<String>,
    #[serde(default = "default_batch")]
    pub consumer_batch: usize,
    #[serde(default = "default_concurrency")]
    pub consumer_concurrency: usize,
    #[serde(default = "default_query_timeout_ms")]
    pub query_timeout_ms: u64,
    /// Matches scored for relevance sort (most recently updated first). See README "Limits".
    #[serde(default = "default_rank_window")]
    pub rank_window: u32,
    /// Matches aggregated for facet counts. Larger match sets report lower bounds. See README "Limits".
    #[serde(default = "default_facet_window")]
    pub facet_window: u32,
    /// Facet counts are reused for identical filters within this TTL; 0 disables the cache.
    #[serde(default = "default_facet_cache_ttl_ms")]
    pub facet_cache_ttl_ms: u64,
    /// `work_mem` for the query pool; too small makes bitmap scans lossy on broad matches.
    #[serde(default = "default_query_work_mem")]
    pub query_work_mem: String,
    #[serde(default = "default_db_max_connections")]
    pub db_max_connections: u32,
    /// Budget for parsed artifact bodies, by serialized size; 0 disables the cache.
    #[serde(default = "default_artifact_cache_mb")]
    pub artifact_cache_mb: usize,
    /// Shards a new rebuild is split into; processes claim one shard at a time.
    #[serde(default = "default_rebuild_shards")]
    pub rebuild_shards: u32,
    /// Documents re-indexed concurrently by this process during a rebuild; 0 opts out.
    #[serde(default = "default_rebuild_concurrency")]
    pub rebuild_concurrency: usize,
}

fn default_bind() -> String {
    "0.0.0.0:8082".into()
}
fn default_mapping_path() -> String {
    "config/mapping.toml".into()
}
fn default_batch() -> usize {
    256
}
fn default_concurrency() -> usize {
    16
}
fn default_query_timeout_ms() -> u64 {
    2000
}
fn default_rank_window() -> u32 {
    2000
}
fn default_facet_window() -> u32 {
    100_000
}
fn default_facet_cache_ttl_ms() -> u64 {
    10_000
}
fn default_query_work_mem() -> String {
    "32MB".into()
}
fn default_db_max_connections() -> u32 {
    20
}
fn default_artifact_cache_mb() -> usize {
    256
}
fn default_rebuild_shards() -> u32 {
    32
}
fn default_rebuild_concurrency() -> usize {
    8
}

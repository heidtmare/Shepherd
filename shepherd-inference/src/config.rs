use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind_addr: String,
    pub database_url: String,
    pub nats_url: String,
    pub storage_url: String,
    #[serde(default = "default_rules_path")]
    pub rules_path: String,
    /// Interval of the loop that retries `inference.findings.v1` writes.
    #[serde(default = "default_sync_secs")]
    pub sync_secs: u64,
    #[serde(default = "default_db_max_connections")]
    pub db_max_connections: u32,
    /// Projection events evaluated at once. Events for one document are handled in order.
    #[serde(default = "default_consumer_concurrency")]
    pub consumer_concurrency: usize,
    /// `inference.findings.v1` writes in flight after evaluations; the rest wait for the retry loop.
    #[serde(default = "default_write_concurrency")]
    pub write_concurrency: usize,
    /// An A-Number held by more cases than this is not joined across cases.
    #[serde(default = "default_a_number_case_limit")]
    pub a_number_case_limit: i64,
}

fn default_bind() -> String {
    "0.0.0.0:8087".into()
}
fn default_rules_path() -> String {
    "config/rules.toml".into()
}
fn default_sync_secs() -> u64 {
    15
}
fn default_db_max_connections() -> u32 {
    10
}
fn default_consumer_concurrency() -> usize {
    4
}
fn default_write_concurrency() -> usize {
    2
}
fn default_a_number_case_limit() -> i64 {
    50
}

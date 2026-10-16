use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind_addr: String,
    pub nats_url: String,
    #[serde(default = "default_topology_path")]
    pub topology_path: String,
    /// Bearer token for DLQ replay/delete. Those routes return 401 when unset.
    pub admin_token: Option<String>,
    /// Comma-separated browser origins allowed to call the API (e.g. the intake upload form).
    #[serde(default)]
    pub cors_allowed_origins: String,
}

fn default_bind() -> String {
    "0.0.0.0:8084".into()
}
fn default_topology_path() -> String {
    "config/topology.toml".into()
}

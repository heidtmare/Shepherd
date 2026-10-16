use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind_addr: String,
    pub database_url: String,
    pub nats_url: String,
    pub storage_url: String,
    /// Intake base URL, for re-running recognition with a chosen form type.
    pub intake_url: Option<String>,
    #[serde(default = "default_forms_dir")]
    pub forms_dir: String,
    #[serde(default = "default_pdfium_dir")]
    pub pdfium_lib_dir: String,
    /// Resolution of page images served to the workbench.
    #[serde(default = "default_page_dpi")]
    pub page_dpi: f32,
    /// A claim older than this may be taken over by another user.
    #[serde(default = "default_claim_ttl")]
    pub claim_ttl_minutes: i64,
    /// Open a task when recognition matches no template (the reviewer picks the form type).
    #[serde(default = "default_true")]
    pub open_unrecognized: bool,
    #[serde(default = "default_db_max_connections")]
    pub db_max_connections: u32,
    /// shepherd-search base URL, for case discovery. Unset falls back to database lookup.
    pub search_url: Option<String>,
    /// shepherd-notification base URL, for case letters. Unset hides correspondence.
    pub notification_url: Option<String>,
    /// shepherd-inference base URL, for case findings. Unset hides findings and skips the approval
    /// check.
    pub inference_url: Option<String>,
    /// Benefit definitions: case facts, criteria and denial grounds per form type.
    #[serde(default = "default_benefits_path")]
    pub benefits_path: String,
    /// Three-letter prefix of generated receipt numbers.
    #[serde(default = "default_receipt_prefix")]
    pub receipt_prefix: String,
    /// Default response window for a request for evidence.
    #[serde(default = "default_rfe_days")]
    pub rfe_response_days: i64,
    /// Default response window for a notice of intent to deny.
    #[serde(default = "default_noid_days")]
    pub noid_response_days: i64,
    /// Interval of the retry loop that writes pending `workbench.case.v1` artifacts.
    #[serde(default = "default_case_sync_secs")]
    pub case_sync_secs: u64,
    /// `max_cases` of a worker registered without one.
    #[serde(default = "default_workload_max_cases")]
    pub workload_default_max_cases: i32,
    /// Interval of the sweep that assigns unassigned cases by rule.
    #[serde(default = "default_workload_sweep_secs")]
    pub workload_sweep_secs: u64,
}

fn default_bind() -> String {
    "0.0.0.0:8085".into()
}
fn default_forms_dir() -> String {
    "forms".into()
}
fn default_pdfium_dir() -> String {
    "vendor/pdfium/lib".into()
}
fn default_page_dpi() -> f32 {
    110.0
}
fn default_claim_ttl() -> i64 {
    30
}
fn default_true() -> bool {
    true
}
fn default_db_max_connections() -> u32 {
    10
}
fn default_benefits_path() -> String {
    "config/benefits.toml".into()
}
fn default_receipt_prefix() -> String {
    "SHP".into()
}
fn default_rfe_days() -> i64 {
    87
}
fn default_noid_days() -> i64 {
    33
}
fn default_case_sync_secs() -> u64 {
    15
}
fn default_workload_max_cases() -> i32 {
    10
}
fn default_workload_sweep_secs() -> u64 {
    60
}

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind_addr: String,
    pub nats_url: String,
    /// shepherd-storage base URL used by this service.
    pub storage_url: String,
    /// Browser-facing base URLs used by the upload form. Default to local compose ports.
    #[serde(default = "default_public_storage")]
    pub public_storage_url: String,
    #[serde(default = "default_public_search")]
    pub public_search_url: String,
    #[serde(default = "default_public_stream")]
    pub public_stream_url: String,
    #[serde(default = "default_public_workbench")]
    pub public_workbench_url: String,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_extract_timeout_secs")]
    pub extract_timeout_secs: u64,
    /// Per-page text cap in characters; bounds IR size for pathological pages.
    #[serde(default = "default_max_page_chars")]
    pub max_page_chars: usize,
    /// Handwritten form recognition (`intake.form.v1`). Requires pdfium, templates and a model.
    #[serde(default = "default_true")]
    pub recognition_enabled: bool,
    #[serde(default = "default_forms_dir")]
    pub forms_dir: String,
    #[serde(default = "default_pdfium_dir")]
    pub pdfium_lib_dir: String,
    #[serde(default = "default_htr_dir")]
    pub htr_model_dir: String,
    /// Parallel recognitions; each already uses all cores for inference.
    #[serde(default = "default_recognition_concurrency")]
    pub recognition_concurrency: usize,
    /// A timed-out recognition keeps its slot until it finishes; see README "Recognition".
    #[serde(default = "default_recognition_timeout_secs")]
    pub recognition_timeout_secs: u64,
    #[serde(default = "default_recognition_blank_cache_mb")]
    pub recognition_blank_cache_mb: usize,
}

fn default_true() -> bool {
    true
}
fn default_forms_dir() -> String {
    "forms".into()
}
fn default_pdfium_dir() -> String {
    "vendor/pdfium/lib".into()
}
fn default_htr_dir() -> String {
    "vendor/models/trocr-base-handwritten".into()
}
fn default_recognition_concurrency() -> usize {
    1
}
fn default_recognition_timeout_secs() -> u64 {
    540
}
fn default_recognition_blank_cache_mb() -> usize {
    256
}

fn default_bind() -> String {
    "0.0.0.0:8083".into()
}
fn default_public_storage() -> String {
    "http://localhost:8081".into()
}
fn default_public_search() -> String {
    "http://localhost:8082".into()
}
fn default_public_stream() -> String {
    "http://localhost:8084".into()
}
fn default_public_workbench() -> String {
    "http://localhost:8085".into()
}
fn default_concurrency() -> usize {
    4
}
fn default_extract_timeout_secs() -> u64 {
    120
}
fn default_max_page_chars() -> usize {
    200_000
}

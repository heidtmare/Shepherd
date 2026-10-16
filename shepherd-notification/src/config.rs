use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind_addr: String,
    pub database_url: String,
    pub nats_url: String,
    pub storage_url: String,
    /// shepherd-workbench base URL: case facts, benefit and e-notification contact.
    pub workbench_url: String,
    #[serde(default = "default_letters_path")]
    pub letters_path: String,
    /// SMTP server, e.g. `smtp://mailpit:1025` or `smtps://user:pass@host`. Unset leaves e-mail
    /// deliveries queued.
    pub smtp_url: Option<String>,
    #[serde(default = "default_email_from")]
    pub email_from: String,
    /// Interval of the loop that retries held letters and sends due e-mail.
    #[serde(default = "default_retry_secs")]
    pub retry_secs: u64,
    /// E-mail attempts before a delivery is marked failed.
    #[serde(default = "default_email_max_attempts")]
    pub email_max_attempts: i32,
    #[serde(default = "default_db_max_connections")]
    pub db_max_connections: u32,
}

fn default_bind() -> String {
    "0.0.0.0:8086".into()
}
fn default_letters_path() -> String {
    "config/letters.toml".into()
}
fn default_email_from() -> String {
    "Shepherd Notices <notices@shepherd.invalid>".into()
}
fn default_retry_secs() -> u64 {
    60
}
fn default_email_max_attempts() -> i32 {
    5
}
fn default_db_max_connections() -> u32 {
    5
}

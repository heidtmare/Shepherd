use std::process::ExitCode;

use shepherd_common::{config, healthcheck, http, telemetry};
use shepherd_intake::Config;
use tokio_util::sync::CancellationToken;

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        let bind = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8083".into());
        return healthcheck::probe(&bind);
    }
    telemetry::init_tracing();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(serve()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "fatal");
            ExitCode::FAILURE
        }
    }
}

async fn serve() -> anyhow::Result<()> {
    let cfg: Config = config::from_env()?;
    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await?;
    let shutdown = CancellationToken::new();
    http::cancel_on_signal(shutdown.clone());
    shepherd_intake::run(cfg, listener, shutdown).await
}

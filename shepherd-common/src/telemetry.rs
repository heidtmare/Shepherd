use std::sync::OnceLock;

pub use metrics_exporter_prometheus::PrometheusHandle;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Installs the global tracing subscriber (default filter `info,ort=warn`; override with `RUST_LOG`). `LOG_FORMAT=pretty` selects human-readable output; default is JSON.
pub fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,ort=warn"));
    let pretty = std::env::var("LOG_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("pretty"));
    let registry = tracing_subscriber::registry().with(filter);
    let res = if pretty {
        registry.with(fmt::layer()).try_init()
    } else {
        registry
            .with(fmt::layer().json().flatten_event(true))
            .try_init()
    };
    let _ = res;
}

/// Global Prometheus recorder; installed on first call so multiple apps can share a process (tests).
pub fn metrics_handle() -> PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .set_buckets_for_metric(
                    Matcher::Suffix("_seconds".into()),
                    &[
                        0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
                        10.0,
                    ],
                )
                .expect("static bucket config")
                .install_recorder()
                .expect("metrics recorder already installed by another library")
        })
        .clone()
}

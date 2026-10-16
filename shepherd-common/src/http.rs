use std::time::Instant;

use axum::{
    extract::{MatchedPath, Request},
    middleware::Next,
    response::Response,
};
use tokio_util::sync::CancellationToken;

/// Records `http_requests_total` and `http_request_duration_seconds`, labelled by route template.
pub async fn track_metrics(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".into());
    let method = req.method().to_string();
    let res = next.run(req).await;
    let status = res.status().as_u16().to_string();
    let labels = [("method", method), ("path", path), ("status", status)];
    metrics::counter!("http_requests_total", &labels).increment(1);
    metrics::histogram!("http_request_duration_seconds", &labels)
        .record(start.elapsed().as_secs_f64());
    res
}

/// Cancels `token` on SIGINT or SIGTERM.
pub fn cancel_on_signal(token: CancellationToken) {
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        let _ = ctrl_c.await;
        tracing::info!("shutdown signal received");
        token.cancel();
    });
}

/// Constant-time check of `Authorization: Bearer <token>`. Always false when `expected` is `None`.
pub fn bearer_matches(headers: &axum::http::HeaderMap, expected: Option<&str>) -> bool {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match (expected, presented) {
        (Some(e), Some(p)) => {
            e.len() == p.len()
                && e.bytes()
                    .zip(p.bytes())
                    .fold(0u8, |acc, (x, y)| acc | (x ^ y))
                    == 0
        }
        _ => false,
    }
}

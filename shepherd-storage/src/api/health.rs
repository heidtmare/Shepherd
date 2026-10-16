use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};

use super::AppState;

pub async fn healthz() -> &'static str {
    "ok"
}

pub async fn readyz(State(st): State<AppState>) -> (StatusCode, Json<Value>) {
    let db = sqlx::query("SELECT 1").execute(&st.db).await.is_ok();
    let blob = st.blobs.ping().await.is_ok();
    let nats = match &st.nats {
        Some(c) => c.connection_state() == async_nats::connection::State::Connected,
        None => true,
    };
    let status = if db && blob && nats {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({ "db": db, "blob": blob, "nats": nats })),
    )
}

pub async fn metrics(State(st): State<AppState>) -> String {
    st.metrics.render()
}

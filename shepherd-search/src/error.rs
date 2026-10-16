use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::query::QueryError;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error(transparent)]
    Query(#[from] QueryError),
    #[error("not found")]
    NotFound,
    #[error("unauthorized")]
    Unauthorized,
    #[error("{0}")]
    Conflict(&'static str),
    #[error("query timed out")]
    Timeout,
    #[error(transparent)]
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        // Postgres 57014 = query_canceled (statement_timeout).
        let timed_out = e
            .downcast_ref::<sqlx::Error>()
            .and_then(|e| e.as_database_error())
            .and_then(|d| d.code())
            .is_some_and(|c| c == "57014");
        if timed_out {
            ApiError::Timeout
        } else {
            ApiError::Internal(e)
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            ApiError::Query(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ApiError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            ApiError::Timeout => (StatusCode::SERVICE_UNAVAILABLE, "timeout"),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        let message = if let ApiError::Internal(e) = &self {
            tracing::error!(error = format!("{e:#}"), "request failed");
            "internal error".to_owned()
        } else {
            self.to_string()
        };
        (status, Json(json!({ "error": code, "message": message }))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

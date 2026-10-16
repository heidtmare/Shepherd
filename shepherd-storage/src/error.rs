use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::blob::BlobError;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    UnsupportedMediaType(String),
    #[error("payload exceeds {0} bytes")]
    TooLarge(u64),
    /// Transient; the client should retry.
    #[error("{0}")]
    Unavailable(String),
    #[error("range not satisfiable")]
    RangeNotSatisfiable { size: u64 },
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Blob(#[from] object_store::Error),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<BlobError> for ApiError {
    fn from(e: BlobError) -> Self {
        match e {
            BlobError::TooLarge(n) => ApiError::TooLarge(n),
            BlobError::Empty => ApiError::BadRequest("empty body".into()),
            BlobError::BadPrefix => ApiError::UnsupportedMediaType("body is not a PDF".into()),
            BlobError::Body(e) => ApiError::BadRequest(format!("request body: {e}")),
            BlobError::Store(e) => ApiError::Blob(e),
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::UnsupportedMediaType(_) => {
                (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type")
            }
            ApiError::TooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
            ApiError::RangeNotSatisfiable { size } => {
                return (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(axum::http::header::CONTENT_RANGE, format!("bytes */{size}"))],
                )
                    .into_response();
            }
            ApiError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            ApiError::Db(sqlx::Error::RowNotFound) => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::Blob(object_store::Error::NotFound { .. }) => {
                (StatusCode::NOT_FOUND, "not_found")
            }
            ApiError::Db(_) | ApiError::Blob(_) | ApiError::Internal(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            }
        };
        let message = if status.is_server_error() && !matches!(self, ApiError::Unavailable(_)) {
            tracing::error!(error = %self, "request failed");
            "internal error".to_owned()
        } else {
            self.to_string()
        };
        (
            status,
            Json(ErrorBody {
                error: code,
                message,
            }),
        )
            .into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

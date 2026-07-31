use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

pub trait DomainError: std::error::Error + Send + Sync + 'static {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

pub enum ApiError {
    Domain(Box<dyn DomainError>),
    Conflict,   // StoreError::Conflict after retries exhausted
    Overloaded, // command queue is full
    NotFound(Option<String>),
    BadRequest(Option<String>),
    Internal(anyhow::Error),
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::Domain(e) => (e.status_code(), e.to_string()),
            ApiError::Conflict => (
                StatusCode::CONFLICT,
                "aggregate was modified concurrently, please retry".to_string(),
            ),
            ApiError::Overloaded => (
                StatusCode::SERVICE_UNAVAILABLE,
                "system is busy, please retry shortly".to_string(),
            ),
            ApiError::Internal(e) => {
                tracing::error!(error = %e, "internal error handling command");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
            ApiError::NotFound(message) => (
                StatusCode::NOT_FOUND,
                message.unwrap_or_else(|| "not found".to_string()),
            ),
            ApiError::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                message.unwrap_or_else(|| "bad request".to_string()),
            ),
        };
        (status, Json(ErrorBody { error: message })).into_response()
    }
}

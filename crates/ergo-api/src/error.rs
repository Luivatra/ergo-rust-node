//! API error types.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use thiserror::Error;

/// API errors.
#[derive(Error, Debug)]
pub enum ApiError {
    /// Not found.
    #[error("Not found: {0}")]
    NotFound(String),

    /// Bad request.
    #[error("Bad request: {0}")]
    BadRequest(String),

    /// Internal error.
    #[error("Internal error: {0}")]
    Internal(String),

    /// Unauthorized.
    #[error("Unauthorized")]
    Unauthorized,

    /// Not implemented.
    #[error("Not implemented: {0}")]
    NotImplemented(String),

    /// State error.
    #[error("State error: {0}")]
    State(#[from] ergo_state::StateError),

    /// Mempool error.
    #[error("Mempool error: {0}")]
    Mempool(#[from] ergo_mempool::MempoolError),

    /// Consensus error.
    #[error("Consensus error: {0}")]
    Consensus(#[from] ergo_consensus::ConsensusError),
}

/// Error response body.
#[derive(Serialize)]
struct ErrorResponse {
    error: u16,
    reason: String,
    detail: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error_response) = match &self {
            ApiError::NotFound(msg) => (
                StatusCode::NOT_FOUND,
                ErrorResponse {
                    error: 404,
                    reason: "Not Found".to_string(),
                    detail: msg.clone(),
                },
            ),
            ApiError::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                ErrorResponse {
                    error: 400,
                    reason: "Bad Request".to_string(),
                    detail: msg.clone(),
                },
            ),
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                ErrorResponse {
                    error: 401,
                    reason: "Unauthorized".to_string(),
                    detail: "API key required".to_string(),
                },
            ),
            ApiError::NotImplemented(msg) => (
                StatusCode::NOT_IMPLEMENTED,
                ErrorResponse {
                    error: 501,
                    reason: "Not Implemented".to_string(),
                    detail: msg.clone(),
                },
            ),
            ApiError::Internal(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorResponse {
                    error: 500,
                    reason: "Internal Server Error".to_string(),
                    detail: msg.clone(),
                },
            ),
            ApiError::State(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorResponse {
                    error: 500,
                    reason: "State Error".to_string(),
                    detail: e.to_string(),
                },
            ),
            ApiError::Mempool(e) => (
                StatusCode::BAD_REQUEST,
                ErrorResponse {
                    error: 400,
                    reason: "Mempool Error".to_string(),
                    detail: e.to_string(),
                },
            ),
            ApiError::Consensus(e) => (
                StatusCode::BAD_REQUEST,
                ErrorResponse {
                    error: 400,
                    reason: "Validation Error".to_string(),
                    detail: e.to_string(),
                },
            ),
        };

        (status, Json(error_response)).into_response()
    }
}

/// Result type for API operations.
pub type ApiResult<T> = Result<T, ApiError>;

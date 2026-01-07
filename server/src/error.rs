//! Error types for the application

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Invalid path: {0}")]
    InvalidPath(String),

    #[error("Band not found: {0}")]
    BandNotFound(String),

    #[error("Image processing error: {0}")]
    ImageProcessing(String),

    #[error("External API error: {0}")]
    ExternalApi(String),

    #[error("HTTP client error: {0}")]
    HttpClient(#[from] reqwest::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::BandNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            AppError::InvalidPath(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            AppError::ImageProcessing(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
            AppError::ExternalApi(_) | AppError::HttpClient(_) => {
                (StatusCode::BAD_GATEWAY, self.to_string())
            }
        };

        (status, message).into_response()
    }
}

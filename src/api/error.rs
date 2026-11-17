use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::db::objects::Kind;

/// An HTTP error with a JSON body.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub body: Value,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({"error": message.into()}),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    /// Braintrust answers 403 for objects that are missing or deleted.
    pub fn missing(kind: Kind, id: &str) -> Self {
        let message = format!(
            "Missing read access to {} id {id}, or the {} does not exist",
            kind.as_str(),
            kind.as_str()
        );
        Self {
            status: StatusCode::FORBIDDEN,
            body: json!({"Code": "ForbiddenError", "Message": message}),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

impl From<crate::error::Error> for ApiError {
    fn from(err: crate::error::Error) -> Self {
        tracing::error!(%err, "request failed");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
    }
}

impl From<crate::ingest::IngestError> for ApiError {
    fn from(err: crate::ingest::IngestError) -> Self {
        match err {
            crate::ingest::IngestError::Invalid(message) => Self::bad_request(message),
            crate::ingest::IngestError::Db(err) => err.into(),
        }
    }
}

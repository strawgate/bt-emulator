//! Request helpers: lenient JSON bodies, repeated-key query strings, base URLs.

use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use axum::http::{HeaderMap, header};
use serde_json::Value;

use super::{ApiError, ApiResult};

/// A JSON body parsed regardless of Content-Type (Python posts `/logs3` without
/// one). An empty body is `null`.
pub struct JsonBody(pub Value);

impl<S: Send + Sync> FromRequest<S> for JsonBody {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|e| ApiError::bad_request(e.body_text()))?;
        if bytes.trim_ascii().is_empty() {
            return Ok(Self(Value::Null));
        }
        serde_json::from_slice(&bytes)
            .map(Self)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))
    }
}

/// Query parameters, preserving repeated keys (`ids=a&ids=b`).
#[derive(Debug, Default)]
pub struct Params(Vec<(String, String)>);

impl Params {
    pub fn parse(query: Option<&str>) -> Self {
        Self(
            form_urlencoded::parse(query.unwrap_or_default().as_bytes())
                .into_owned()
                .collect(),
        )
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn all(&self, key: &str) -> impl Iterator<Item = &str> {
        self.0
            .iter()
            .filter(move |(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn number(&self, key: &str) -> ApiResult<Option<usize>> {
        self.get(key)
            .map(|v| {
                v.parse().map_err(|_| {
                    ApiError::bad_request(format!("`{key}` must be a non-negative integer"))
                })
            })
            .transpose()
    }
}

/// The emulator's own base URL as the client addressed it.
pub fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    format!("http://{host}")
}

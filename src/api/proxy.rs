//! The AI proxy (`/v1/proxy/*`, and the OpenAI paths under `/v1`): requests are
//! forwarded unchanged to the OpenAI-compatible API given by `--llm-base-url`, and
//! the response is streamed back. See docs/protocol.md §20.

use std::sync::Arc;

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::Path;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

use super::{ApiError, ApiResult};
use crate::invoke::Runtime;

/// `/v1/proxy/<path>`.
pub async fn proxied(
    Extension(rt): Extension<Arc<Runtime>>,
    method: Method,
    Path(path): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    forward(&rt, method, &path, uri.query(), &headers, body).await
}

/// `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`, `/v1/responses`.
pub async fn openai(
    Extension(rt): Extension<Arc<Runtime>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let path = uri.path().trim_start_matches("/v1/").to_owned();
    forward(&rt, method, &path, uri.query(), &headers, body).await
}

async fn forward(
    rt: &Runtime,
    method: Method,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let path = match query {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    };
    // The caller authenticated with a Braintrust key; the model gets its own.
    let Some(mut req) = rt.model_request(method, &path) else {
        return Err(ApiError::bad_request(
            "the AI proxy needs a model: start bt-emulator with --llm-base-url pointing at an OpenAI-compatible API",
        ));
    };
    let forwarded = [header::CONTENT_TYPE, header::ACCEPT]
        .into_iter()
        .filter_map(|name| Some((name.clone(), headers.get(&name)?.clone())));
    for (name, value) in forwarded {
        req = req.header(name, value);
    }
    let req = req.body(body);
    let resp = req.send().await.map_err(|e| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("model request failed: {e}"),
        )
    })?;
    let status = resp.status();
    let content_type = resp.headers().get(header::CONTENT_TYPE).cloned();
    let mut out = Body::from_stream(resp.bytes_stream()).into_response();
    *out.status_mut() = status;
    if let Some(content_type) = content_type {
        out.headers_mut().insert(header::CONTENT_TYPE, content_type);
    }
    Ok(out)
}

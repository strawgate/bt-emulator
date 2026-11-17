//! `POST /otel/v1/traces`: OTLP/HTTP trace export, protobuf or JSON, optionally
//! gzipped. Each span is written as a row of the object its parent names: the
//! span's `braintrust.parent` attribute, else the `x-bt-parent` header.
//! See docs/protocol.md §17.

use std::collections::HashMap;
use std::io::Read;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use super::objects::{LogTarget, log_fields};
use super::{ApiError, ApiResult, MAX_BODY_BYTES};
use crate::db::Db;

pub async fn traces(State(db): State<Db>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let mut bytes = body.to_vec();
    if header("content-encoding") == Some("gzip") {
        bytes = gunzip(&bytes, MAX_BODY_BYTES)?;
    }
    let is_json = header("content-type").is_some_and(|t| t.starts_with("application/json"));
    let spans = if is_json {
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))?;
        crate::otlp::decode_json(&body)
    } else {
        crate::otlp::decode_protobuf(&bytes)
    }
    .map_err(ApiError::bad_request)?;

    let default_parent = header("x-bt-parent");
    let mut destinations: HashMap<String, Map<String, Value>> = HashMap::new();
    let mut rows = Vec::with_capacity(spans.len());
    for span in &spans {
        // OTLP: an empty or all-zero span id is invalid; rows are keyed by it.
        if span.span_id.trim_start_matches('0').is_empty() {
            return Err(ApiError::bad_request(
                "span has an empty or all-zero spanId",
            ));
        }
        let (parent, mut row) = crate::otlp::to_row(span);
        let parent = parent.as_deref().or(default_parent).ok_or_else(|| {
            ApiError::bad_request(
                "span has no parent: set the braintrust.parent attribute or the x-bt-parent header",
            )
        })?;
        if !destinations.contains_key(parent) {
            let fields = log_fields(&db, &target(parent)?).await?;
            destinations.insert(parent.to_owned(), fields);
        }
        row.extend(destinations[parent].clone());
        rows.push(Value::Object(row));
    }
    crate::ingest::write(&db, rows, None).await?;
    Ok(if is_json {
        axum::Json(json!({})).into_response()
    } else {
        // An empty ExportTraceServiceResponse.
        (
            [(header::CONTENT_TYPE, "application/x-protobuf")],
            Vec::new(),
        )
            .into_response()
    })
}

/// Inflate a gzipped body, refusing one that inflates past `limit` bytes (the body
/// limit caps only the compressed size).
fn gunzip(bytes: &[u8], limit: usize) -> ApiResult<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(limit as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| ApiError::bad_request(format!("invalid gzip body: {e}")))?;
    if out.len() > limit {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("the decompressed body is larger than {limit} bytes"),
        ));
    }
    Ok(out)
}

/// The object a parent string (`experiment_id:<id>`, `project_id:<id>` or
/// `project_name:<name>`) names.
fn target(parent: &str) -> ApiResult<LogTarget> {
    match parent.split_once(':') {
        Some(("experiment_id", id)) => Ok(LogTarget::Experiment(id.to_owned())),
        Some(("project_id", id)) => Ok(LogTarget::ProjectLogs(id.to_owned())),
        Some(("project_name", name)) => Ok(LogTarget::NamedProjectLogs(name.to_owned())),
        _ => Err(ApiError::bad_request(format!(
            "unsupported parent {parent:?}: expected experiment_id:, project_id: or project_name:"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_strings_name_their_target() {
        let ok = [
            ("experiment_id:e1", LogTarget::Experiment("e1".into())),
            ("project_id:p1", LogTarget::ProjectLogs("p1".into())),
            // Only the first colon separates: names may contain colons.
            (
                "project_name:a:b",
                LogTarget::NamedProjectLogs("a:b".into()),
            ),
        ];
        for (parent, want) in ok {
            assert_eq!(target(parent).unwrap(), want);
        }
        for bad in ["", "experiment_id", "dataset_id:d", "nope:x"] {
            let err = target(bad).unwrap_err();
            assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST, "{bad}");
        }
    }

    #[test]
    fn gzip_bodies_are_capped_after_inflating() {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(&[0; 100]).unwrap();
        let gz = enc.finish().unwrap();
        assert_eq!(gunzip(&gz, 100).unwrap(), [0; 100]);
        let err = gunzip(&gz, 99).unwrap_err();
        assert_eq!(err.status, StatusCode::PAYLOAD_TOO_LARGE);
        let err = gunzip(b"not gzip", 100).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }
}

//! `POST /otel/v1/traces`: OTLP/HTTP trace export, protobuf or JSON, optionally
//! gzipped. Each span is written as a row of the object its parent names: the
//! span's `braintrust.parent` attribute, else the `x-bt-parent` header.
//! See docs/protocol.md §17.

use std::collections::HashMap;
use std::io::Read;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::objects::{get_or_create_project, require};
use super::{ApiError, ApiResult};
use crate::db::Db;
use crate::db::objects::Kind;

pub async fn traces(State(db): State<Db>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let mut bytes = body.to_vec();
    if header("content-encoding") == Some("gzip") {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes.as_slice())
            .read_to_end(&mut out)
            .map_err(|e| ApiError::bad_request(format!("invalid gzip body: {e}")))?;
        bytes = out;
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
    let mut destinations: HashMap<String, Value> = HashMap::new();
    let mut rows = Vec::with_capacity(spans.len());
    for span in &spans {
        let (parent, mut row) = crate::otlp::to_row(span);
        let parent = parent.as_deref().or(default_parent).ok_or_else(|| {
            ApiError::bad_request(
                "span has no parent: set the braintrust.parent attribute or the x-bt-parent header",
            )
        })?;
        if !destinations.contains_key(parent) {
            destinations.insert(parent.to_owned(), destination(&db, parent).await?);
        }
        row.extend(
            destinations[parent]
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
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

/// The row fields addressing the object a parent string names.
async fn destination(db: &Db, parent: &str) -> ApiResult<Value> {
    match parent.split_once(':') {
        Some(("experiment_id", id)) => {
            require(db, Kind::Experiment, id).await?;
            Ok(json!({"experiment_id": id}))
        }
        Some(("project_id", id)) => {
            require(db, Kind::Project, id).await?;
            Ok(json!({"project_id": id, "log_id": "g"}))
        }
        Some(("project_name", name)) => {
            let (project, _) = get_or_create_project(db, name, &Value::Null).await?;
            Ok(json!({"project_id": project.id(), "log_id": "g"}))
        }
        _ => Err(ApiError::bad_request(format!(
            "unsupported parent {parent:?}: expected experiment_id:, project_id: or project_name:"
        ))),
    }
}

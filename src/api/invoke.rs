//! `POST /v1/function/{id}/invoke` and the SDKs' `POST /function/invoke`, which
//! names the function by id, by project and slug, or as a global function.
//! See docs/protocol.md §16.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use super::extract::JsonBody;
use super::functions::{Flavor, OBJECT_TYPE, current};
use super::objects::{get_or_create_project, require};
use super::rows::parse_version;
use super::{ApiError, ApiResult, ORG_ID};
use crate::db::Db;
use crate::db::objects::Kind;
use crate::invoke::{Call, InvokeError, Runtime};
use crate::span_parent::Parent;

pub async fn by_id(
    State(db): State<Db>,
    Extension(rt): Extension<Arc<Runtime>>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Response> {
    let version = parse_version(&body["version"])?;
    let function = current(&db, Flavor::Function, &id, version).await?;
    run(&db, &rt, function.data, &body).await
}

pub async fn by_ref(
    State(db): State<Db>,
    Extension(rt): Extension<Arc<Runtime>>,
    JsonBody(body): JsonBody,
) -> ApiResult<Response> {
    let version = parse_version(&body["version"])?;
    let data = if let Some(id) = body["function_id"].as_str() {
        current(&db, Flavor::Function, id, version).await?.data
    } else if let Some(slug) = body["slug"].as_str() {
        let project_id = match (body["project_id"].as_str(), body["project_name"].as_str()) {
            (Some(id), _) => id.to_owned(),
            (None, Some(name)) => db
                .object_by_name(Kind::Project, ORG_ID, name)
                .await?
                .ok_or_else(|| ApiError::bad_request(format!("project {name:?} not found")))?
                .id()
                .to_owned(),
            (None, None) => {
                return Err(ApiError::bad_request(
                    "`slug` needs `project_name` or `project_id`",
                ));
            }
        };
        db.rows_page(OBJECT_TYPE, &project_id, version, None, usize::MAX)
            .await?
            .into_iter()
            .find(|e| e.field("slug") == slug)
            .ok_or_else(|| ApiError::bad_request(format!("no function with slug {slug:?}")))?
            .data
    } else if body["global_function"].is_string() {
        return Err(ApiError::bad_request(
            "global functions (autoevals) are not available in bt-emulator",
        ));
    } else {
        return Err(ApiError::bad_request(
            "name the function with `function_id`, or `project_name` and `slug`",
        ));
    };
    run(&db, &rt, data, &body).await
}

/// The row fields addressing where an invoked function's span is logged, or
/// `None` when the call has no parent (or one the emulator does not store).
async fn destination(db: &Db, parent: &Value) -> ApiResult<Option<(Parent, Value)>> {
    if parent.is_null() || parent == "" {
        return Ok(None);
    }
    let parent = crate::span_parent::parse(parent).map_err(ApiError::bad_request)?;
    let fields = match (
        parent.object_type.as_str(),
        &parent.object_id,
        &parent.project_name,
    ) {
        ("experiment", Some(id), _) => {
            require(db, Kind::Experiment, id).await?;
            json!({"experiment_id": id})
        }
        ("project_logs", Some(id), _) => {
            require(db, Kind::Project, id).await?;
            json!({"project_id": id, "log_id": "g"})
        }
        ("project_logs", None, Some(name)) => {
            let (project, _) = get_or_create_project(db, name, &Value::Null).await?;
            json!({"project_id": project.id(), "log_id": "g"})
        }
        _ => return Ok(None),
    };
    Ok(Some((parent, fields)))
}

/// Log the call as a `function` span under its parent.
async fn log_span(
    db: &Db,
    (parent, fields): (Parent, Value),
    function: &Map<String, Value>,
    body: &Value,
    result: &Result<Value, InvokeError>,
    (start, end): (f64, f64),
) -> ApiResult<()> {
    let span_id = uuid::Uuid::new_v4().to_string();
    let mut row = json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "span_id": span_id,
        "root_span_id": span_id,
        "input": body["input"],
        "metadata": body["metadata"],
        "tags": body["tags"],
        "span_attributes": {"name": function.get("name").cloned().unwrap_or(Value::Null), "type": "function"},
        "metrics": {"start": start, "end": end},
    });
    match result {
        Ok(output) => row["output"] = output.clone(),
        Err(InvokeError::Unsupported(e) | InvokeError::Failed(e)) => row["error"] = json!(e),
    }
    if let Some((_, parent_span, root)) = &parent.row {
        row["span_parents"] = json!([parent_span]);
        row["root_span_id"] = json!(root);
    }
    let mut row: Map<String, Value> = serde_json::from_value(row).expect("row is an object");
    if let Some(Value::Object(event)) = parent.propagated_event {
        crate::merge::merge_row(&mut row, event, &[]);
    }
    row.extend(fields.as_object().cloned().unwrap_or_default());
    crate::ingest::write(db, vec![Value::Object(row)], None).await?;
    Ok(())
}

fn now_secs() -> f64 {
    chrono::Utc::now().timestamp_micros() as f64 / 1e6
}

async fn run(
    db: &Db,
    rt: &Runtime,
    function: Map<String, Value>,
    body: &Value,
) -> ApiResult<Response> {
    let call = Call {
        input: body["input"].clone(),
        messages: body["messages"].as_array().cloned().unwrap_or_default(),
        metadata: body["metadata"].clone(),
    };
    // Resolve the parent first, so a bad one fails before the function runs.
    let destination = destination(db, &body["parent"]).await?;
    let start = now_secs();
    let result = rt.invoke(&function, &call).await;
    if let Some(destination) = destination {
        log_span(
            db,
            destination,
            &function,
            body,
            &result,
            (start, now_secs()),
        )
        .await?;
    }
    let output = result.map_err(|e| match e {
        InvokeError::Unsupported(message) => ApiError::bad_request(message),
        InvokeError::Failed(message) => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, message),
    })?;
    if body["stream"] != true {
        return Ok(axum::Json(output).into_response());
    }
    // Braintrust's SSE format: start, one delta with the whole output, done.
    let delta = match &output {
        Value::String(_) => format!("event: text_delta\ndata: {output}\n\n"),
        other => format!("event: json_delta\ndata: {other}\n\n"),
    };
    let body = format!("event: start\ndata: \n\n{delta}event: done\ndata: \n\n");
    Ok(([(header::CONTENT_TYPE, "text/event-stream")], body).into_response())
}

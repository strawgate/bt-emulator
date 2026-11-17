//! `POST /v1/function/{id}/invoke` and the SDKs' `POST /function/invoke`, which
//! names the function by id, by project and slug, or as a global function.
//! See docs/protocol.md §16.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use super::extract::JsonBody;
use super::functions::{Flavor, OBJECT_TYPE, current};
use super::rows::parse_version;
use super::{ApiError, ApiResult, ORG_ID};
use crate::db::Db;
use crate::db::objects::Kind;
use crate::invoke::{Call, InvokeError, Runtime};

pub async fn by_id(
    State(db): State<Db>,
    Extension(rt): Extension<Arc<Runtime>>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Response> {
    let version = parse_version(&body["version"])?;
    let function = current(&db, Flavor::Function, &id, version).await?;
    run(&rt, function.data, &body).await
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
            .find(|e| e.data["slug"] == slug)
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
    run(&rt, data, &body).await
}

async fn run(
    rt: &Runtime,
    function: serde_json::Map<String, Value>,
    body: &Value,
) -> ApiResult<Response> {
    let call = Call {
        input: body["input"].clone(),
        messages: body["messages"].as_array().cloned().unwrap_or_default(),
        metadata: body["metadata"].clone(),
    };
    let output = rt.invoke(&function, &call).await.map_err(|e| match e {
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

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
use super::functions::{Flavor, by_slug, current};
use super::objects::{LogTarget, log_fields, project_ref};
use super::rows::parse_version;
use super::{ApiError, ApiResult};
use crate::db::Db;
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
        let (id, name) = (body["project_id"].as_str(), body["project_name"].as_str());
        if id.is_none() && name.is_none() {
            return Err(ApiError::bad_request(
                "`slug` needs `project_name` or `project_id`",
            ));
        }
        let project = project_ref(&db, id, name, false).await?;
        by_slug(&db, project.id(), slug, version)
            .await?
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

/// Where a parent asks for the span to be logged: `None` for playground logs,
/// which the emulator does not store.
fn target(parent: &Parent) -> Result<Option<LogTarget>, String> {
    match (
        parent.object_type.as_str(),
        &parent.object_id,
        &parent.project_name,
    ) {
        ("experiment", Some(id), _) => Ok(Some(LogTarget::Experiment(id.clone()))),
        ("project_logs", Some(id), _) => Ok(Some(LogTarget::ProjectLogs(id.clone()))),
        ("project_logs", None, Some(name)) => Ok(Some(LogTarget::NamedProjectLogs(name.clone()))),
        ("playground_logs", ..) => Ok(None),
        (object_type, ..) => Err(format!(
            "cannot log under parent {object_type:?} without an id: expected an experiment or project_logs parent"
        )),
    }
}

/// The parent an invoke names and the row fields addressing where its span goes, or
/// `None` when there is no parent, or one the emulator does not store.
async fn destination(db: &Db, parent: &Value) -> ApiResult<Option<(Parent, Map<String, Value>)>> {
    if parent.is_null() || parent == "" {
        return Ok(None);
    }
    let parent = crate::span_parent::parse(parent).map_err(ApiError::bad_request)?;
    let Some(target) = target(&parent).map_err(ApiError::bad_request)? else {
        return Ok(None);
    };
    let fields = log_fields(db, &target).await?;
    Ok(Some((parent, fields)))
}

/// The `function` span recording one call, nested under `parent`'s row if it names
/// one, with the parent's propagated fields merged in.
fn span_row(
    parent: Parent,
    function: &Map<String, Value>,
    body: &Value,
    result: &Result<Value, InvokeError>,
    (start, end): (f64, f64),
) -> Map<String, Value> {
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
        Err(
            InvokeError::Unsupported(e)
            | InvokeError::Invalid(e)
            | InvokeError::Model(e)
            | InvokeError::Failed(e),
        ) => row["error"] = json!(e),
    }
    if let Some((_, parent_span, root)) = &parent.row {
        row["span_parents"] = json!([parent_span]);
        row["root_span_id"] = json!(root);
    }
    let mut row: Map<String, Value> = serde_json::from_value(row).expect("row is an object");
    if let Some(Value::Object(event)) = parent.propagated_event {
        crate::merge::merge_row(&mut row, event, &[]);
    }
    row
}

fn now_secs() -> f64 {
    chrono::Utc::now().timestamp_micros() as f64 / 1e6
}

/// Braintrust's SSE format for an invoke: start, one delta with the whole output, done.
fn sse_body(output: &Value) -> String {
    let event = if output.is_string() {
        "text_delta"
    } else {
        "json_delta"
    };
    format!("event: start\ndata: \n\nevent: {event}\ndata: {output}\n\nevent: done\ndata: \n\n")
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
    if let Some((parent, fields)) = destination {
        let mut row = span_row(parent, &function, body, &result, (start, now_secs()));
        row.extend(fields);
        crate::ingest::write(db, vec![Value::Object(row)], None).await?;
    }
    let output = result.map_err(|e| match e {
        InvokeError::Unsupported(message) | InvokeError::Invalid(message) => {
            ApiError::bad_request(message)
        }
        InvokeError::Model(message) => ApiError::new(StatusCode::BAD_GATEWAY, message),
        InvokeError::Failed(message) => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, message),
    })?;
    if body["stream"] != true {
        return Ok(axum::Json(output).into_response());
    }
    Ok((
        [(header::CONTENT_TYPE, "text/event-stream")],
        sse_body(&output),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent(object_type: &str, id: Option<&str>, name: Option<&str>) -> Parent {
        Parent {
            object_type: object_type.into(),
            object_id: id.map(str::to_owned),
            project_name: name.map(str::to_owned),
            ..Parent::default()
        }
    }

    #[test]
    fn targets_follow_the_parent_object() {
        let cases = [
            (
                parent("experiment", Some("e"), None),
                Some(LogTarget::Experiment("e".into())),
            ),
            // An id wins over a name.
            (
                parent("project_logs", Some("p"), Some("n")),
                Some(LogTarget::ProjectLogs("p".into())),
            ),
            (
                parent("project_logs", None, Some("n")),
                Some(LogTarget::NamedProjectLogs("n".into())),
            ),
            // Playground logs are not stored, so their spans are dropped.
            (parent("playground_logs", Some("x"), None), None),
        ];
        for (parent, want) in cases {
            assert_eq!(target(&parent), Ok(want), "{parent:?}");
        }
        // Anything else cannot be logged anywhere, so it is refused, not dropped.
        for bad in [
            parent("experiment", None, Some("n")),
            parent("project_logs", None, None),
            parent("", None, None),
            parent("dataset", Some("d"), None),
        ] {
            assert!(target(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn span_rows_record_the_call() {
        let function: Map<String, Value> =
            serde_json::from_value(json!({"name": "greet"})).unwrap();
        let body = json!({"input": {"q": 1}, "metadata": {"m": 2}, "tags": ["t"]});
        let row = span_row(
            Parent::default(),
            &function,
            &body,
            &Ok(json!("hi")),
            (1.5, 2.5),
        );
        assert_eq!(row["input"], json!({"q": 1}));
        assert_eq!(row["metadata"], json!({"m": 2}));
        assert_eq!(row["tags"], json!(["t"]));
        assert_eq!(row["output"], "hi");
        assert!(!row.contains_key("error"));
        assert_eq!(
            row["span_attributes"],
            json!({"name": "greet", "type": "function"})
        );
        assert_eq!(row["metrics"], json!({"start": 1.5, "end": 2.5}));
        // Without a parent row the span is its own root.
        assert_eq!(row["root_span_id"], row["span_id"]);
        assert!(!row.contains_key("span_parents"));
        assert_ne!(row["id"], row["span_id"]);
    }

    #[test]
    fn span_rows_nest_under_the_parent_and_carry_its_event() {
        let parent = Parent {
            row: Some(("row".into(), "pspan".into(), "proot".into())),
            propagated_event: Some(json!({"metadata": {"from": "parent"}, "tags": ["p"]})),
            ..Parent::default()
        };
        let body = json!({"metadata": {"m": 2}});
        let failed = Err(InvokeError::Failed("boom".into()));
        let row = span_row(parent, &Map::new(), &body, &failed, (0.0, 0.0));
        assert_eq!(row["span_parents"], json!(["pspan"]));
        assert_eq!(row["root_span_id"], "proot");
        assert_ne!(row["span_id"], "pspan");
        assert_eq!(row["error"], "boom");
        assert!(!row.contains_key("output"));
        assert_eq!(row["span_attributes"]["name"], Value::Null);
        // Propagated fields merge into the row: objects deep-merge, others replace.
        assert_eq!(row["metadata"], json!({"m": 2, "from": "parent"}));
        assert_eq!(row["tags"], json!(["p"]));
    }

    #[test]
    fn sse_bodies_match_the_sdk_parser() {
        // The SDKs JSON.parse a text_delta's data and take a json_delta's verbatim.
        assert_eq!(
            sse_body(&json!("a \"b\"\nc")),
            "event: start\ndata: \n\nevent: text_delta\ndata: \"a \\\"b\\\"\\nc\"\n\nevent: done\ndata: \n\n"
        );
        assert_eq!(
            sse_body(&json!({"score": 1})),
            "event: start\ndata: \n\nevent: json_delta\ndata: {\"score\":1}\n\nevent: done\ndata: \n\n"
        );
        // A JSON-encoded delta never spans lines, which would split the SSE event.
        assert!(!sse_body(&json!({"s": "x\ny"})).contains("x\ny"));
    }
}

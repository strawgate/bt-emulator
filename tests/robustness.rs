//! Malformed input never crashes a handler. For each endpoint outside the OpenAPI
//! spec (which `just fuzz` covers), a valid request is mutated one field at a time
//! (wrong types, nulls, removal, nesting) and every response must be below 500.

mod common;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::{Map, Value, json};
use tower::ServiceExt;

async fn send(
    db: &bt_emulator::db::Db,
    method: &str,
    uri: &str,
    body: String,
    ty: &str,
) -> (u16, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", "Bearer t")
        .header("content-type", ty)
        .body(Body::from(body))
        .unwrap();
    let resp = bt_emulator::api::router(db.clone())
        .oneshot(req)
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn ok(db: &bt_emulator::db::Db, method: &str, uri: &str, body: Value) -> Value {
    let (status, text) = send(db, method, uri, body.to_string(), "application/json").await;
    assert_eq!(status, 200, "{uri}: {text}");
    serde_json::from_str(&text).unwrap()
}

/// Replacement values that are wrong for most fields.
fn replacements() -> Vec<Value> {
    vec![
        Value::Null,
        json!(0),
        json!(-1),
        json!(1.5e300),
        json!(true),
        json!(""),
        json!("x"),
        json!("00000000-0000-0000-0000-000000000000"),
        json!([]),
        json!([null]),
        json!({}),
        json!({"a": {"b": [1, "2", null]}}),
    ]
}

/// Every one-field mutation of `body`, recursing one level into objects and into
/// the first element of arrays.
fn mutations(body: &Value) -> Vec<Value> {
    let mut out = vec![Value::Null, json!([]), json!("text"), json!({})];
    let Value::Object(fields) = body else {
        return out;
    };
    for (key, value) in fields {
        let mut removed = fields.clone();
        removed.remove(key);
        out.push(Value::Object(removed));
        for replacement in replacements() {
            let mut changed = fields.clone();
            changed.insert(key.clone(), replacement);
            out.push(Value::Object(changed));
        }
        let nested = match value {
            Value::Object(_) => Some(value.clone()),
            Value::Array(items) => items.first().cloned(),
            _ => None,
        };
        if let Some(inner @ Value::Object(_)) = nested {
            for m in mutations(&inner) {
                let mut changed: Map<String, Value> = fields.clone();
                changed.insert(key.clone(), if value.is_array() { json!([m]) } else { m });
                out.push(Value::Object(changed));
            }
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_requests_never_cause_server_errors() {
    for (backend, db) in common::backends().await {
        let pid = ok(&db, "POST", "/v1/project", json!({"name": "p"})).await["id"].clone();
        let eid = ok(
            &db,
            "POST",
            "/v1/experiment",
            json!({"project_id": pid, "name": "e"}),
        )
        .await["id"]
            .clone();
        let did = ok(
            &db,
            "POST",
            "/v1/dataset",
            json!({"project_id": pid, "name": "d"}),
        )
        .await["id"]
            .clone();
        let fid = ok(&db, "POST", "/v1/function", json!({"project_id": pid, "name": "f", "slug": "f",
            "function_data": {"type": "prompt"}, "prompt_data": {"prompt": {"type": "chat", "messages": []}}}))
        .await["id"]
            .clone();
        let (e, d, f) = (
            eid.as_str().unwrap(),
            did.as_str().unwrap(),
            fid.as_str().unwrap(),
        );
        let otlp = json!({"resourceSpans": [{"scopeSpans": [{"spans": [{"traceId": "aa", "spanId": "bb",
            "name": "s", "startTimeUnixNano": "1", "endTimeUnixNano": "2",
            "attributes": [{"key": "braintrust.parent", "value": {"stringValue": format!("experiment_id:{e}")}}]}]}]}]});
        let endpoints: Vec<(&str, String, Value)> = vec![
            (
                "POST",
                "/logs3".into(),
                json!({"api_version": 2, "rows": [{"id": "r", "experiment_id": e, "input": 1, "_is_merge": false, "metrics": {"start": 1}}]}),
            ),
            (
                "POST",
                "/btql".into(),
                json!({"query": {"from": {"op": "function", "name": {"op": "ident", "name": ["experiment"]}, "args": [{"op": "literal", "value": e}]},
                "select": [{"op": "star"}], "filter": {"op": "eq", "left": {"op": "ident", "name": ["id"]}, "right": {"op": "literal", "value": "r"}}, "limit": 1, "cursor": null}, "fmt": "json"}),
            ),
            (
                "POST",
                "/btql".into(),
                json!({"query": format!("SELECT * FROM experiment('{e}') WHERE id = 'r' LIMIT 5"), "fmt": "jsonl"}),
            ),
            ("POST", "/otel/v1/traces".into(), otlp),
            (
                "POST",
                "/attachment".into(),
                json!({"key": "k", "filename": "f", "content_type": "text/plain", "org_id": "o"}),
            ),
            (
                "POST",
                "/attachment/status".into(),
                json!({"key": "k", "org_id": "o", "status": {"upload_status": "done"}}),
            ),
            (
                "POST",
                "/function/invoke".into(),
                json!({"function_id": f, "input": {"a": 1}, "parent": {"object_type": "experiment", "object_id": e}, "stream": false, "version": null}),
            ),
            (
                "POST",
                format!("/v1/function/{f}/invoke"),
                json!({"input": "x", "messages": [{"role": "user", "content": "x"}], "metadata": {}}),
            ),
            (
                "POST",
                "/function/code".into(),
                json!({"org_id": "o", "runtime_context": {"runtime": "node", "version": "22"}}),
            ),
            (
                "POST",
                "/insert-functions".into(),
                json!({"functions": [{"project_id": pid, "name": "g", "slug": "g", "function_data": {"type": "prompt"}, "if_exists": "replace"}]}),
            ),
            (
                "POST",
                format!("/v1/dataset/{d}/restore/preview"),
                json!({"version": "1"}),
            ),
            (
                "POST",
                "/api/dataset_snapshot/register".into(),
                json!({"dataset_id": d, "dataset_snapshot_name": "s", "description": null, "xact_id": "1", "update": false}),
            ),
            (
                "POST",
                "/api/dataset_snapshot/get".into(),
                json!({"dataset_id": d, "name": "s"}),
            ),
            (
                "POST",
                "/api/project/register".into(),
                json!({"project_name": "p", "org_id": "o"}),
            ),
            ("POST", "/api/project/get".into(), json!({"id": pid})),
            (
                "POST",
                "/api/experiment/register".into(),
                json!({"project_name": "p", "experiment_name": "e2", "repo_info": {"branch": "b"}, "ensure_new": true, "dataset_id": d}),
            ),
            ("POST", "/api/experiment/get".into(), json!({"id": e})),
            (
                "POST",
                "/api/base_experiment/get_id".into(),
                json!({"id": e}),
            ),
            (
                "POST",
                "/api/dataset/register".into(),
                json!({"project_name": "p", "dataset_name": "d"}),
            ),
            (
                "POST",
                "/api/self/get_object_info".into(),
                json!({"object_type": "experiment", "object_ids": [e], "accept_arbitrary_acl_object_types": true}),
            ),
            (
                "POST",
                "/v1/proxy/chat/completions".into(),
                json!({"model": "m", "messages": []}),
            ),
        ];
        let mut tried = 0;
        for (method, uri, valid) in &endpoints {
            for body in mutations(valid) {
                for ty in ["application/json", "text/plain"] {
                    let (status, text) = send(&db, method, uri, body.to_string(), ty).await;
                    assert!(
                        status < 500,
                        "{backend} {method} {uri} {body} -> {status}: {text}"
                    );
                    tried += 1;
                }
            }
            // Bytes that are not JSON at all.
            for raw in ["", "{", "\u{0}\u{1}", "[1,2", "\"unterminated"] {
                let (status, text) = send(&db, method, uri, raw.into(), "application/json").await;
                assert!(
                    status < 500,
                    "{backend} {method} {uri} {raw:?} -> {status}: {text}"
                );
            }
        }
        assert!(tried > 1000, "{tried}");
    }
}

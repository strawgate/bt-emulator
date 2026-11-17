//! OTLP trace ingest at `/otel/v1/traces`, run against every backend.

mod common;

use std::io::Write;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bt_emulator::otlp::proto;
use http_body_util::BodyExt;
use prost::Message;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(
    db: &bt_emulator::db::Db,
    req: axum::http::request::Builder,
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    let req = req
        .header("authorization", "Bearer t")
        .body(Body::from(body))
        .unwrap();
    let resp = bt_emulator::api::router(db.clone())
        .oneshot(req)
        .await
        .unwrap();
    let status = resp.status();
    (
        status,
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
}

async fn json_call(db: &bt_emulator::db::Db, method: &str, path: &str, body: Value) -> Value {
    let req = Request::builder().method(method).uri(path);
    let (status, bytes) = call(db, req, body.to_string().into_bytes()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{path}: {}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}

fn traces() -> axum::http::request::Builder {
    Request::builder().method("POST").uri("/otel/v1/traces")
}

fn attr(key: &str, value: &str) -> proto::KeyValue {
    proto::KeyValue {
        key: key.into(),
        value: Some(proto::AnyValue {
            value: Some(proto::any_value::Value::StringValue(value.into())),
        }),
    }
}

fn export(spans: Vec<proto::Span>) -> Vec<u8> {
    proto::ExportTraceServiceRequest {
        resource_spans: vec![proto::ResourceSpans {
            scope_spans: vec![proto::ScopeSpans { spans }],
        }],
    }
    .encode_to_vec()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap()
}

#[tokio::test]
async fn otlp_spans_become_rows() {
    for (_, db) in common::backends().await {
        let pid =
            json_call(&db, "POST", "/v1/project", json!({"name": "otel"})).await["id"].clone();
        let eid = json_call(
            &db,
            "POST",
            "/v1/experiment",
            json!({"project_id": pid, "name": "e"}),
        )
        .await["id"]
            .as_str()
            .unwrap()
            .to_owned();

        // Protobuf, gzipped, as the Go SDK exports: the parent is a span attribute.
        let parent = format!("experiment_id:{eid}");
        let root = proto::Span {
            trace_id: vec![0xaa; 16],
            span_id: vec![0x01; 8],
            name: "eval".into(),
            start_time_unix_nano: 1_700_000_000_000_000_000,
            end_time_unix_nano: 1_700_000_001_000_000_000,
            attributes: vec![
                attr("braintrust.parent", &parent),
                attr("braintrust.input_json", "{\"q\": \"a\"}"),
                attr("braintrust.output_json", "\"A\""),
                attr("braintrust.span_attributes", "{\"type\": \"eval\"}"),
            ],
            ..Default::default()
        };
        let child = proto::Span {
            span_id: vec![0x02; 8],
            parent_span_id: vec![0x01; 8],
            name: "exact".into(),
            attributes: vec![
                attr("braintrust.parent", &parent),
                attr("braintrust.scores", "{\"exact\": 1}"),
            ],
            ..root.clone()
        };
        let req = traces()
            .header("content-type", "application/x-protobuf")
            .header("content-encoding", "gzip");
        let (status, body) = call(&db, req, gzip(&export(vec![root, child]))).await;
        assert_eq!((status, body), (StatusCode::OK, vec![]));
        let events = json_call(
            &db,
            "POST",
            &format!("/v1/experiment/{eid}/fetch"),
            json!({}),
        )
        .await["events"]
            .clone();
        let by_id = |id: &str| {
            events
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["id"] == id)
                .unwrap()
                .clone()
        };
        let root = by_id("0101010101010101");
        assert_eq!(root["root_span_id"], "aa".repeat(16));
        assert_eq!(
            (root["input"].clone(), root["output"].clone()),
            (json!({"q": "a"}), json!("A"))
        );
        assert_eq!(
            (
                root["span_attributes"]["type"].clone(),
                root["span_attributes"]["name"].clone()
            ),
            (json!("eval"), json!("eval"))
        );
        assert_eq!(
            root["metrics"],
            json!({"start": 1_700_000_000.0, "end": 1_700_000_001.0, "duration": 1.0})
        );
        assert_eq!(root["created"], "2023-11-14T22:13:20.000Z");
        assert_eq!(root["is_root"], true);
        let child = by_id("0202020202020202");
        assert_eq!(
            (child["span_parents"].clone(), child["scores"].clone()),
            (json!(["0101010101010101"]), json!({"exact": 1}))
        );

        // OTLP/JSON with the parent in the x-bt-parent header; project_name creates the project.
        let span = json!({"traceId": "0b", "spanId": "0c", "name": "llm", "startTimeUnixNano": "0", "endTimeUnixNano": "0",
            "attributes": [{"key": "gen_ai.prompt", "value": {"stringValue": "hi"}}]});
        let body = json!({"resourceSpans": [{"scopeSpans": [{"spans": [span]}]}]});
        let req = traces()
            .header("content-type", "application/json")
            .header("x-bt-parent", "project_name:fresh");
        let (status, out) = call(&db, req, body.to_string().into_bytes()).await;
        assert_eq!(
            (status, serde_json::from_slice::<Value>(&out).unwrap()),
            (StatusCode::OK, json!({}))
        );
        let fresh =
            json_call(&db, "GET", "/v1/project?project_name=fresh", Value::Null).await["objects"]
                [0]["id"]
                .as_str()
                .unwrap()
                .to_owned();
        let logs = json_call(
            &db,
            "POST",
            &format!("/v1/project_logs/{fresh}/fetch"),
            json!({}),
        )
        .await["events"]
            .clone();
        assert_eq!(
            (logs[0]["id"].clone(), logs[0]["input"].clone()),
            (json!("0c"), json!("hi"))
        );

        // project_id parent: the attribute wins over the header.
        let span = proto::Span {
            span_id: vec![0x03; 8],
            attributes: vec![attr(
                "braintrust.parent",
                &format!("project_id:{}", pid.as_str().unwrap()),
            )],
            ..Default::default()
        };
        let req = traces().header("x-bt-parent", "project_name:ignored");
        assert_eq!(call(&db, req, export(vec![span])).await.0, StatusCode::OK);
        let logs = json_call(
            &db,
            "POST",
            &format!("/v1/project_logs/{}/fetch", pid.as_str().unwrap()),
            json!({}),
        )
        .await;
        assert_eq!(logs["events"][0]["id"], "0303030303030303");
        let names = json_call(&db, "GET", "/v1/project?project_name=ignored", Value::Null).await;
        assert_eq!(names["objects"], json!([]));
    }
}

#[tokio::test]
async fn otlp_rejects_bad_exports() {
    let (_, db) = common::backends().await.remove(0);
    let span = |parent: &str| {
        let attributes = if parent.is_empty() {
            vec![]
        } else {
            vec![attr("braintrust.parent", parent)]
        };
        export(vec![proto::Span {
            span_id: vec![0x01; 8],
            attributes,
            ..Default::default()
        }])
    };
    let missing = format!("experiment_id:{}", uuid::Uuid::new_v4());
    let pid = json_call(&db, "POST", "/v1/project", json!({"name": "p"})).await["id"].clone();
    let parent = format!("project_id:{}", pid.as_str().unwrap());
    let span_id = |id: Vec<u8>| {
        export(vec![proto::Span {
            span_id: id,
            attributes: vec![attr("braintrust.parent", &parent)],
            ..Default::default()
        }])
    };
    for (req, body, status, needle) in [
        // Rows are keyed by span id, which OTLP requires to be non-zero.
        (
            traces(),
            span_id(vec![]),
            StatusCode::BAD_REQUEST,
            "empty or all-zero spanId",
        ),
        (
            traces(),
            span_id(vec![0; 8]),
            StatusCode::BAD_REQUEST,
            "empty or all-zero spanId",
        ),
        (traces(), span(""), StatusCode::BAD_REQUEST, "no parent"),
        (
            traces(),
            span("playground_id:x"),
            StatusCode::BAD_REQUEST,
            "unsupported parent",
        ),
        (
            traces(),
            span(&missing),
            StatusCode::FORBIDDEN,
            "Missing read access",
        ),
        (
            traces(),
            b"\xff".to_vec(),
            StatusCode::BAD_REQUEST,
            "invalid OTLP protobuf",
        ),
        (
            traces().header("content-encoding", "gzip"),
            b"nope".to_vec(),
            StatusCode::BAD_REQUEST,
            "invalid gzip",
        ),
        (
            traces().header("content-type", "application/json"),
            b"{".to_vec(),
            StatusCode::BAD_REQUEST,
            "invalid JSON",
        ),
        (
            traces().header("content-type", "application/json"),
            b"[]".to_vec(),
            StatusCode::BAD_REQUEST,
            "must be an object",
        ),
    ] {
        let (got, out) = call(&db, req, body).await;
        let out = String::from_utf8_lossy(&out);
        assert_eq!(got, status, "{out}");
        assert!(out.contains(needle), "{out}");
    }
}

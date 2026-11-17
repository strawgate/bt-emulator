//! `braintrust push`: bundle uploads and `/insert-functions`, on every backend.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(
    db: &bt_emulator::db::Db,
    method: &str,
    uri: &str,
    body: Value,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "emu.test")
        .header("authorization", "Bearer t")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = bt_emulator::api::router(db.clone())
        .oneshot(req)
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    (
        status,
        serde_json::from_str(&text).unwrap_or_else(|_| json!(text)),
    )
}

fn prompt(pid: &Value, if_exists: Option<&str>, text: &str) -> Value {
    let mut f = json!({"project_id": pid, "name": "Greeter", "slug": "greet",
        "function_data": {"type": "prompt"},
        "prompt_data": {"prompt": {"type": "chat", "messages": [{"role": "user", "content": text}]}}});
    if let Some(mode) = if_exists {
        f["if_exists"] = json!(mode);
    }
    f
}

#[tokio::test]
async fn push_uploads_bundles_and_inserts_functions() {
    for (_, db) in common::backends().await {
        // A bundle upload URL, usable without the API key.
        let (status, code) = call(
            &db,
            "POST",
            "/function/code",
            json!({"org_id": "o", "runtime_context": {"runtime": "node", "version": "22"}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let bundle_id = code["bundleId"].as_str().unwrap();
        let url = code["url"].as_str().unwrap();
        assert!(
            url.starts_with(&format!(
                "http://emu.test/attachment/blob/{bundle_id}?token="
            )),
            "{url}"
        );
        let (status, _) = call(&db, "POST", "/function/code", json!({"org_id": "o"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let pid = call(&db, "POST", "/v1/project", json!({"name": "p"}))
            .await
            .1["id"]
            .clone();
        let bundle = json!({"project_id": pid, "name": "double", "slug": "double", "function_type": "tool",
            "function_data": {"type": "code", "data": {"type": "bundle", "bundle_id": bundle_id,
                "runtime_context": {"runtime": "node", "version": "22"}, "location": {"type": "function", "index": 0}}},
            "if_exists": "replace"});
        let (status, out) = call(
            &db,
            "POST",
            "/insert-functions",
            json!({"functions": [prompt(&pid, None, "v1"), bundle]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{out}");
        assert_eq!(out["status"], "success");
        let created = out["functions"].as_array().unwrap();
        assert_eq!(created.len(), 2);
        assert_eq!(
            (
                created[0]["slug"].clone(),
                created[0]["found_existing"].clone()
            ),
            (json!("greet"), json!(false))
        );
        assert_eq!(created[0]["project_id"], pid);
        let id = created[0]["id"].as_str().unwrap().to_owned();
        let stored = call(&db, "GET", &format!("/v1/function/{id}"), Value::Null)
            .await
            .1;
        assert!(stored.get("if_exists").is_none(), "{stored}");
        // The response's xact_id is the newest version written.
        assert!(
            stored["_xact_id"].as_str() <= out["xact_id"].as_str(),
            "{stored} {out}"
        );

        // The default, and "error", refuse an existing slug; "ignore" keeps it; "replace" updates it.
        for mode in [None, Some("error")] {
            let (status, out) = call(
                &db,
                "POST",
                "/insert-functions",
                json!({"functions": [prompt(&pid, mode, "v2")]}),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(out["error"].as_str().unwrap().contains("already exists"));
        }
        let (_, out) = call(
            &db,
            "POST",
            "/insert-functions",
            json!({"functions": [prompt(&pid, Some("ignore"), "v2")]}),
        )
        .await;
        assert_eq!(
            (
                out["functions"][0]["id"].clone(),
                out["functions"][0]["found_existing"].clone()
            ),
            (json!(id), json!(true))
        );
        let content = |v: &Value| v["prompt_data"]["prompt"]["messages"][0]["content"].clone();
        assert_eq!(
            content(
                &call(&db, "GET", &format!("/v1/function/{id}"), Value::Null)
                    .await
                    .1
            ),
            "v1"
        );
        let (_, out) = call(
            &db,
            "POST",
            "/insert-functions",
            json!({"functions": [prompt(&pid, Some("replace"), "v3")]}),
        )
        .await;
        assert_eq!(out["functions"][0]["found_existing"], true);
        assert_eq!(
            content(
                &call(&db, "GET", &format!("/v1/function/{id}"), Value::Null)
                    .await
                    .1
            ),
            "v3"
        );

        // Bundled code is stored but cannot run here.
        let double = created[1]["id"].as_str().unwrap();
        let (status, out) = call(
            &db,
            "POST",
            &format!("/v1/function/{double}/invoke"),
            json!({"input": 1}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(out["error"].as_str().unwrap().contains("bundled code"));

        // Nothing is written when any definition is invalid or any slug that must be
        // new is taken, by an existing function or earlier in the batch.
        let count = || async {
            call(&db, "GET", "/v1/function", Value::Null).await.1["objects"]
                .as_array()
                .unwrap()
                .len()
        };
        let before = count().await;
        let mut fresh = prompt(&pid, Some("replace"), "x");
        fresh["slug"] = json!("fresh");
        let mut twice = prompt(&pid, None, "x");
        twice["slug"] = json!("twice");
        for (functions, needle) in [
            (
                json!([fresh, prompt(&pid, None, "x")]),
                "functions/1: a function with slug \"greet\"",
            ),
            (
                json!([twice.clone(), twice]),
                "functions/1: a function with slug \"twice\"",
            ),
        ] {
            let (status, out) = call(
                &db,
                "POST",
                "/insert-functions",
                json!({"functions": functions}),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{out}");
            assert!(out["error"].as_str().unwrap().contains(needle), "{out}");
        }
        let missing = json!("00000000-0000-0000-0000-000000000000");
        for (body, needle) in [
            (json!({"functions": "x"}), "must be an array"),
            (
                json!({"functions": [prompt(&pid, Some("sometimes"), "x")]}),
                "invalid if_exists",
            ),
            (
                json!({"functions": [prompt(&pid, Some("replace"), "x"), {"slug": "s"}]}),
                "functions/1",
            ),
        ] {
            let (status, out) = call(&db, "POST", "/insert-functions", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{out}");
            assert!(out["error"].as_str().unwrap().contains(needle), "{out}");
        }
        assert_eq!(count().await, before);
        let (status, _) = call(
            &db,
            "POST",
            "/insert-functions",
            json!({"functions": [prompt(&missing, None, "x")]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

//! The AI proxy forwards OpenAI-format requests to the configured model.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use bt_emulator::invoke::Runtime;
use common::{Llm, text_reply};

async fn call(
    runtime: &Runtime,
    method: &str,
    uri: &str,
    body: &str,
) -> (StatusCode, String, String) {
    let (_, db) = common::backends().await.remove(0);
    let config = bt_emulator::api::Config {
        runtime: runtime.clone(),
        ..Default::default()
    };
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", "Bearer braintrust-key")
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let resp = bt_emulator::api::app(db, config)
        .oneshot(req)
        .await
        .unwrap();
    let status = resp.status();
    let ty = resp
        .headers()
        .get("content-type")
        .map(|h| h.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, ty, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn requests_are_forwarded_to_the_model() {
    let llm = Llm::default();
    let mut runtime = Runtime::default();
    runtime.llm_base_url = Some(llm.start().await);
    runtime.llm_api_key = Some("sk-model".into());
    let request = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});

    for uri in ["/v1/proxy/chat/completions?x=1", "/v1/chat/completions"] {
        llm.reply(200, text_reply("hello").to_string());
        let (status, ty, body) = call(&runtime, "POST", uri, &request.to_string()).await;
        // The model's own content type comes back (the stub answers as text).
        assert_eq!(
            (status, ty.as_str()),
            (StatusCode::OK, "text/plain; charset=utf-8"),
            "{uri}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            text_reply("hello")
        );
        let (headers, forwarded) = llm.last();
        assert_eq!(forwarded, request);
        // The model sees its own key, never the caller's Braintrust key.
        assert_eq!(headers["authorization"], "Bearer sk-model");
        assert_eq!(headers["accept"], "application/json");
    }

    // Errors pass through with their status; unknown upstream paths too.
    llm.reply(429, "slow down");
    let (status, _, body) = call(&runtime, "POST", "/v1/chat/completions", "{}").await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::TOO_MANY_REQUESTS, "slow down")
    );
    let (status, ty, _) = call(&runtime, "POST", "/v1/proxy/embeddings", "{}").await;
    assert_eq!((status, ty.as_str()), (StatusCode::NOT_FOUND, ""));

    // Without a key configured, no Authorization header is sent.
    runtime.llm_api_key = None;
    call(&runtime, "POST", "/v1/proxy/chat/completions", "{}").await;
    assert!(llm.last().0.get("authorization").is_none());
}

#[tokio::test]
async fn the_proxy_needs_a_reachable_model() {
    let mut runtime = Runtime::default();
    let (status, _, body) = call(&runtime, "POST", "/v1/responses", "{}").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("--llm-base-url"), "{body}");
    runtime.llm_base_url = Some("http://127.0.0.1:9/v1".into());
    let (status, _, body) = call(&runtime, "POST", "/v1/completions", "{}").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("model request failed"), "{body}");
}

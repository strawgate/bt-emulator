//! Attachment upload and download through signed URLs, run against every backend.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Calls the emulator with an API key required, so unsigned blob access would fail.
async fn call(
    db: &bt_emulator::db::Db,
    method: &str,
    uri: &str,
    auth: bool,
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>, String) {
    let config = bt_emulator::api::Config {
        api_key: Some("k".into()),
        ..Default::default()
    };
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "emu.test");
    if auth {
        req = req.header("authorization", "Bearer k");
    }
    let resp = bt_emulator::api::app(db.clone(), config)
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let ty = resp
        .headers()
        .get("content-type")
        .map(|h| h.to_str().unwrap().to_owned())
        .unwrap_or_default();
    (
        status,
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
        ty,
    )
}

async fn api(
    db: &bt_emulator::db::Db,
    method: &str,
    uri: &str,
    body: Value,
) -> (StatusCode, Value) {
    let (status, bytes, _) = call(db, method, uri, true, body.to_string().into_bytes()).await;
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn attachments_round_trip_through_signed_urls() {
    for (_, db) in common::backends().await {
        let meta =
            json!({"key": "k1", "filename": "a.wav", "content_type": "audio/wav", "org_id": "o"});
        let (status, signed) = api(&db, "POST", "/attachment", meta.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            signed["headers"],
            json!({"Content-Type": "audio/wav", "If-None-Match": "*"})
        );
        let url = signed["signedUrl"].as_str().unwrap();
        let path = url.strip_prefix("http://emu.test").unwrap();
        assert!(path.starts_with("/attachment/blob/k1?token="), "{url}");
        // Asking again hands out the same URL.
        assert_eq!(
            api(&db, "POST", "/attachment", meta).await.1["signedUrl"],
            url
        );

        // The signed URL needs no API key, only its token.
        let (status, _, _) = call(&db, "GET", path, false, vec![]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "not uploaded yet");
        let (status, _, _) = call(&db, "PUT", path, false, b"RIFF".to_vec()).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) =
            call(&db, "PUT", "/attachment/blob/k1?token=wrong", false, vec![]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) = call(&db, "GET", "/attachment/blob/nope", false, vec![]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let status_body = json!({"key": "k1", "org_id": "o", "status": {"upload_status": "done"}});
        assert_eq!(
            api(&db, "POST", "/attachment/status", status_body).await.1,
            json!({"status": "success"})
        );

        let (status, read) = api(
            &db,
            "GET",
            "/attachment?key=k1&filename=a.wav&content_type=audio%2Fwav&org_id=o",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(read["status"], json!({"upload_status": "done"}));
        let download = read["downloadUrl"]
            .as_str()
            .unwrap()
            .strip_prefix("http://emu.test")
            .unwrap()
            .to_owned();
        let (status, bytes, ty) = call(&db, "GET", &download, false, vec![]).await;
        assert_eq!(
            (status, bytes, ty.as_str()),
            (StatusCode::OK, b"RIFF".to_vec(), "audio/wav")
        );

        let (_, external) = api(
            &db,
            "GET",
            "/attachment?url=https%3A%2F%2Fx%2Fa.png&filename=a.png",
            Value::Null,
        )
        .await;
        assert_eq!(
            external,
            json!({"downloadUrl": "https://x/a.png", "status": {"upload_status": "done"}})
        );

        for (method, uri, body, want) in [
            (
                "POST",
                "/attachment",
                json!({"filename": "f", "content_type": "t"}),
                StatusCode::BAD_REQUEST,
            ),
            (
                "POST",
                "/attachment/status",
                json!({"key": "k1", "status": "done"}),
                StatusCode::BAD_REQUEST,
            ),
            (
                "POST",
                "/attachment/status",
                json!({"key": "nope", "status": {}}),
                StatusCode::BAD_REQUEST,
            ),
            (
                "GET",
                "/attachment?filename=f",
                Value::Null,
                StatusCode::BAD_REQUEST,
            ),
            (
                "GET",
                "/attachment?key=nope",
                Value::Null,
                StatusCode::NOT_FOUND,
            ),
        ] {
            assert_eq!(api(&db, method, uri, body).await.0, want, "{method} {uri}");
        }
        // Other endpoints still need the API key.
        let (status, _, _) = call(&db, "GET", "/attachment?key=k1", false, vec![]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}

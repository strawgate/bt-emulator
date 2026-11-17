//! Environments: prompts assigned to environments and loaded by environment.

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
        .header("authorization", "Bearer t")
        .body(Body::from(if body.is_null() {
            String::new()
        } else {
            body.to_string()
        }))
        .unwrap();
    let resp = bt_emulator::api::router(db.clone())
        .oneshot(req)
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn ok(db: &bt_emulator::db::Db, method: &str, uri: &str, body: Value) -> Value {
    let (status, out) = call(db, method, uri, body).await;
    assert_eq!(status, StatusCode::OK, "{method} {uri}: {out}");
    out
}

#[tokio::test]
async fn prompts_load_at_the_version_their_environment_serves() {
    for (_, db) in common::backends().await {
        let env = ok(
            &db,
            "POST",
            "/environment",
            json!({"name": "Production", "slug": "production"}),
        )
        .await;
        assert_eq!(
            (env["slug"].clone(), env["org_id"].clone()),
            (json!("production"), json!(bt_emulator::api::ORG_ID))
        );
        // The slug identifies an environment; listing filters by name.
        assert_eq!(
            ok(
                &db,
                "POST",
                "/environment",
                json!({"name": "Other", "slug": "production"})
            )
            .await["id"],
            env["id"]
        );
        ok(
            &db,
            "POST",
            "/environment",
            json!({"name": "Staging", "slug": "staging"}),
        )
        .await;
        assert_eq!(
            ok(&db, "GET", "/environment?name=Staging", Value::Null).await["objects"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let eid = env["id"].as_str().unwrap();
        let (status, _) = call(
            &db,
            "PATCH",
            &format!("/environment/{eid}"),
            json!({"slug": "staging"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = call(&db, "POST", "/environment", json!({"name": "no slug"})).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "validated against the spec"
        );

        let pid = ok(&db, "POST", "/v1/project", json!({"name": "p"})).await["id"].clone();
        let save = |text: &str, envs: Value| {
            json!({"project_id": pid, "name": "Greeter", "slug": "greet", "environment_slugs": envs,
                "prompt_data": {"prompt": {"type": "chat", "messages": [{"role": "user", "content": text}]}}})
        };
        let text = |v: &Value| v["prompt_data"]["prompt"]["messages"][0]["content"].clone();

        // An unknown environment fails the whole write.
        let (status, out) = call(
            &db,
            "POST",
            "/v1/prompt",
            save("v0", json!(["production", "nope"])),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{out}");
        assert!(
            ok(&db, "GET", "/v1/prompt?slug=greet", Value::Null).await["objects"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let v1 = ok(&db, "POST", "/v1/prompt", save("v1", json!(["production"]))).await;
        assert!(v1.get("environment_slugs").is_none(), "{v1}");
        let id = v1["id"].as_str().unwrap().to_owned();
        ok(&db, "PUT", "/v1/prompt", save("v2", Value::Null)).await;

        // Latest is v2; production still serves v1; staging serves nothing.
        assert_eq!(
            text(&ok(&db, "GET", &format!("/v1/prompt/{id}"), Value::Null).await),
            "v2"
        );
        let listed = ok(
            &db,
            "GET",
            "/v1/prompt?project_name=p&slug=greet&environment=production",
            Value::Null,
        )
        .await;
        assert_eq!(text(&listed["objects"][0]), "v1");
        assert_eq!(
            text(
                &ok(
                    &db,
                    "GET",
                    &format!("/v1/function/{id}?environment=production"),
                    Value::Null
                )
                .await
            ),
            "v1"
        );
        let staging = ok(
            &db,
            "GET",
            "/v1/prompt?slug=greet&environment=staging",
            Value::Null,
        )
        .await;
        assert_eq!(staging["objects"], json!([]));
        let (status, out) = call(
            &db,
            "GET",
            &format!("/v1/prompt/{id}?environment=staging"),
            Value::Null,
        )
        .await;
        assert_eq!(
            (status, out["Code"].clone()),
            (StatusCode::BAD_REQUEST, json!("BadRequestError"))
        );

        // Assigning again moves the environment to the new version.
        ok(
            &db,
            "PUT",
            "/v1/prompt",
            save("v3", json!(["production", "staging"])),
        )
        .await;
        assert_eq!(
            text(
                &ok(
                    &db,
                    "GET",
                    &format!("/v1/prompt/{id}?environment=production"),
                    Value::Null
                )
                .await
            ),
            "v3"
        );

        for uri in [
            format!("/v1/prompt/{id}?environment=nope"),
            "/v1/prompt?environment=nope".to_owned(),
            format!(
                "/v1/prompt/{id}?environment=production&version={}",
                v1["_xact_id"].as_str().unwrap()
            ),
            "/v1/prompt/not-a-uuid?environment=production".to_owned(),
        ] {
            assert_eq!(
                call(&db, "GET", &uri, Value::Null).await.0,
                StatusCode::BAD_REQUEST,
                "{uri}"
            );
        }

        // A re-created environment starts with nothing assigned.
        ok(&db, "DELETE", &format!("/environment/{eid}"), Value::Null).await;
        assert_eq!(
            call(&db, "GET", "/v1/prompt?environment=production", Value::Null)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        ok(
            &db,
            "POST",
            "/environment",
            json!({"name": "Production", "slug": "production"}),
        )
        .await;
        let fresh = ok(&db, "GET", "/v1/prompt?environment=production", Value::Null).await;
        assert_eq!(fresh["objects"], json!([]));

        // Deleting an environment's prompt stops serving it, by id or in lists.
        ok(&db, "DELETE", &format!("/v1/prompt/{id}"), Value::Null).await;
        let gone = ok(&db, "GET", "/v1/prompt?environment=staging", Value::Null).await;
        assert_eq!(gone["objects"], json!([]));
        let (status, _) = call(
            &db,
            "GET",
            &format!("/v1/prompt/{id}?environment=staging"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

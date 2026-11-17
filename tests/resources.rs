//! The spec-driven named resources (`/v1/project_score`, `/v1/role`, …), run
//! against every backend.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

struct App {
    db: bt_emulator::db::Db,
}

impl App {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", "Bearer t");
        let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
        let resp = bt_emulator::api::router(self.db.clone())
            .oneshot(req.body(body).unwrap())
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

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, body) = self.call(method, path, body).await;
        assert_eq!(status, StatusCode::OK, "{method} {path}: {body}");
        body
    }

    async fn ids(&self, path: &str) -> Vec<String> {
        self.ok("GET", path, None).await["objects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["id"].as_str().unwrap().to_owned())
            .collect()
    }
}

async fn apps() -> Vec<App> {
    common::backends()
        .await
        .into_iter()
        .map(|(_, db)| App { db })
        .collect()
}

#[tokio::test]
async fn project_scoped_resources_follow_the_named_object_conventions() {
    for app in apps().await {
        let pid = app
            .ok("POST", "/v1/project", Some(json!({"name": "p"})))
            .await["id"]
            .clone();
        let pid = pid.as_str().unwrap();
        let body = json!({"project_id": pid, "name": "accuracy", "score_type": "slider"});
        let score = app
            .ok("POST", "/v1/project_score", Some(body.clone()))
            .await;
        let id = score["id"].as_str().unwrap().to_owned();
        assert_eq!(score["project_id"], pid);
        assert_eq!(score["user_id"], bt_emulator::api::USER_ID);
        // Unset nullable fields are null; unknown fields are dropped.
        assert_eq!(
            (score["description"].clone(), score["config"].clone()),
            (Value::Null, Value::Null)
        );

        // POST with an existing name returns it unmodified; PUT replaces it in place.
        let mut changed = body.clone();
        changed["description"] = json!("new");
        assert_eq!(
            app.ok("POST", "/v1/project_score", Some(changed.clone()))
                .await,
            score
        );
        let replaced = app.ok("PUT", "/v1/project_score", Some(changed)).await;
        assert_eq!(
            (replaced["id"].clone(), replaced["description"].clone()),
            (json!(id), json!("new"))
        );
        assert_eq!(replaced["created"], score["created"]);
        assert_eq!(
            app.ok("GET", &format!("/v1/project_score/{id}"), None)
                .await,
            replaced
        );

        let other = app
            .ok(
                "PUT",
                "/v1/project_score",
                Some(json!({"project_id": pid, "name": "speed", "score_type": "slider"})),
            )
            .await["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            app.ids("/v1/project_score").await,
            [other.clone(), id.clone()]
        );
        assert_eq!(
            app.ids("/v1/project_score?project_score_name=speed").await,
            std::slice::from_ref(&other)
        );
        assert_eq!(
            app.ids(&format!("/v1/project_score?ids={id}")).await,
            std::slice::from_ref(&id)
        );
        assert_eq!(
            app.ids("/v1/project_score?project_name=p&limit=1").await,
            std::slice::from_ref(&other)
        );
        assert_eq!(
            app.ids(&format!(
                "/v1/project_score?project_id={pid}&score_type=slider"
            ))
            .await
            .len(),
            2
        );
        assert!(
            app.ids("/v1/project_score?project_name=nope")
                .await
                .is_empty()
        );

        // PATCH replaces fields, ignoring server and scoping fields.
        let patched = app
            .ok(
                "PATCH",
                &format!("/v1/project_score/{id}"),
                Some(json!({"description": null, "name": "acc", "project_id": other, "id": "x"})),
            )
            .await;
        assert_eq!(
            (patched["name"].clone(), patched["description"].clone()),
            (json!("acc"), Value::Null)
        );
        assert_eq!(
            (patched["id"].clone(), patched["project_id"].clone()),
            (json!(id), json!(pid))
        );
        let (status, _) = app
            .call(
                "PATCH",
                &format!("/v1/project_score/{id}"),
                Some(json!({"name": "speed"})),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        // Renaming onto its own name is fine.
        app.ok(
            "PATCH",
            &format!("/v1/project_score/{id}"),
            Some(json!({"name": "acc"})),
        )
        .await;

        // A missing parent project is a 400; a missing resource is a 403.
        let missing = "00000000-0000-0000-0000-000000000000";
        let (status, text) = app
            .call(
                "POST",
                "/v1/project_tag",
                Some(json!({"project_id": missing, "name": "t"})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            text.as_str()
                .unwrap()
                .starts_with("Missing read access to project id")
        );

        let deleted = app
            .ok("DELETE", &format!("/v1/project_score/{id}"), None)
            .await;
        assert_eq!(deleted["id"], id.as_str());
        let (status, body) = app
            .call("GET", &format!("/v1/project_score/{id}"), None)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body["Message"],
            format!(
                "Missing read access to project_score id {id}, or the project_score does not exist"
            )
        );
        assert_eq!(app.ids("/v1/project_score").await, [other]);
    }
}

#[tokio::test]
async fn org_and_object_scoped_resources() {
    for app in apps().await {
        // Org scope: names are unique across the org; list members are edited in place.
        let role = app
            .ok(
                "POST",
                "/v1/role",
                Some(json!({"name": "reviewer", "member_roles": ["11111111-1111-1111-1111-111111111111"]})),
            )
            .await;
        let id = role["id"].as_str().unwrap();
        assert_eq!(role["org_id"], bt_emulator::api::ORG_ID);
        assert_eq!(role["deleted_at"], Value::Null);
        let patched = app
            .ok(
                "PATCH",
                &format!("/v1/role/{id}"),
                Some(json!({"add_member_roles": ["22222222-2222-2222-2222-222222222222", "11111111-1111-1111-1111-111111111111"], "remove_member_roles": ["33333333-3333-3333-3333-333333333333"], "add_member_permissions": [{"permission": "read"}]})),
            )
            .await;
        assert_eq!(
            patched["member_roles"],
            json!([
                "22222222-2222-2222-2222-222222222222",
                "11111111-1111-1111-1111-111111111111"
            ])
        );
        assert_eq!(
            patched["member_permissions"],
            json!([{"permission": "read"}])
        );
        let patched = app
            .ok(
                "PATCH",
                &format!("/v1/role/{id}"),
                Some(json!({"remove_member_roles": ["22222222-2222-2222-2222-222222222222"]})),
            )
            .await;
        assert_eq!(
            patched["member_roles"],
            json!(["11111111-1111-1111-1111-111111111111"])
        );
        let deleted = app.ok("DELETE", &format!("/v1/role/{id}"), None).await;
        assert!(deleted["deleted_at"].is_string());

        // Required lists default to empty.
        let group = app
            .ok("POST", "/v1/project_group", Some(json!({"name": "g"})))
            .await;
        assert_eq!(group["member_projects"], json!([]));

        // AI secrets are also deleted by name, and gain `updated_at` on every write.
        let secret = app
            .ok(
                "POST",
                "/v1/ai_secret",
                Some(json!({"name": "OPENAI_API_KEY", "secret": "sk", "org_name": "o"})),
            )
            .await;
        assert!(secret.get("secret").is_none(), "{secret}");
        let sid = secret["id"].as_str().unwrap();
        let patched = app
            .ok(
                "PATCH",
                &format!("/v1/ai_secret/{sid}"),
                Some(json!({"type": "openai"})),
            )
            .await;
        assert_eq!(patched["type"], "openai");
        assert!(patched["updated_at"].as_str() >= secret["updated_at"].as_str());
        let gone = app
            .ok(
                "DELETE",
                "/v1/ai_secret",
                Some(json!({"name": "OPENAI_API_KEY"})),
            )
            .await;
        assert_eq!(gone["id"], sid);
        let (status, _) = app
            .call(
                "DELETE",
                "/v1/ai_secret",
                Some(json!({"name": "OPENAI_API_KEY"})),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Object scope: views of one object do not collide with another's.
        let pid = app
            .ok("POST", "/v1/project", Some(json!({"name": "v"})))
            .await["id"]
            .clone();
        let view = |object_id: &Value| json!({"object_type": "project", "object_id": object_id, "view_type": "experiments", "name": "mine"});
        let a = app.ok("POST", "/v1/view", Some(view(&pid))).await;
        let b = app
            .ok(
                "POST",
                "/v1/view",
                Some(view(&json!(bt_emulator::api::ORG_ID))),
            )
            .await;
        assert_ne!(a["id"], b["id"]);
        let listed = app
            .ids(&format!(
                "/v1/view?object_type=project&object_id={}",
                pid.as_str().unwrap()
            ))
            .await;
        assert_eq!(listed, [a["id"].as_str().unwrap()]);

        // Dataset scope: the dataset must exist.
        let did = app
            .ok(
                "POST",
                "/v1/dataset",
                Some(json!({"project_id": pid, "name": "d"})),
            )
            .await["id"]
            .clone();
        let snap = json!({"dataset_id": did, "name": "s", "description": null, "xact_id": "1"});
        app.ok("POST", "/v1/dataset_snapshot", Some(snap)).await;
        let orphan = json!({"dataset_id": "00000000-0000-0000-0000-000000000000", "name": "s", "description": null, "xact_id": "1"});
        let (status, text) = app.call("POST", "/v1/dataset_snapshot", Some(orphan)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            text.as_str()
                .unwrap()
                .starts_with("Missing read access to dataset id")
        );
        let (status, _) = app.call("GET", "/v1/view/not-a-uuid", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

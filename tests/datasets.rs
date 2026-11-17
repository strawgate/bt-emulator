//! Dataset restore and snapshots, on every backend.

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
    let text = String::from_utf8_lossy(&bytes);
    (
        status,
        serde_json::from_str(&text).unwrap_or_else(|_| json!(text)),
    )
}

async fn ok(db: &bt_emulator::db::Db, method: &str, uri: &str, body: Value) -> Value {
    let (status, out) = call(db, method, uri, body).await;
    assert_eq!(status, StatusCode::OK, "{method} {uri}: {out}");
    out
}

async fn contents(db: &bt_emulator::db::Db, did: &str) -> Vec<(String, Value)> {
    let events =
        ok(db, "POST", &format!("/v1/dataset/{did}/fetch"), json!({})).await["events"].clone();
    let mut rows: Vec<_> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["id"].as_str().unwrap().to_owned(), e["input"].clone()))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

#[tokio::test]
async fn datasets_restore_to_an_earlier_version() {
    for (_, db) in common::backends().await {
        let pid = ok(&db, "POST", "/v1/project", json!({"name": "p"})).await["id"].clone();
        let did = ok(
            &db,
            "POST",
            "/v1/dataset",
            json!({"project_id": pid, "name": "d"}),
        )
        .await["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let insert = |events: Value| {
            let (db, did) = (db.clone(), did.clone());
            async move {
                ok(
                    &db,
                    "POST",
                    &format!("/v1/dataset/{did}/insert"),
                    json!({"events": events}),
                )
                .await
            }
        };
        insert(json!([{"id": "a", "input": 1}, {"id": "b", "input": 2}, {"id": "c", "input": 3}]))
            .await;
        let v1 = ok(&db, "GET", "/xact-id", Value::Null).await;
        let v1 = v1.as_str().map_or_else(|| v1.to_string(), str::to_owned);
        let before = contents(&db, &did).await;

        // Change a, delete b, add d; c is untouched.
        insert(json!([{"id": "a", "input": 10}, {"id": "b", "_object_delete": true}, {"id": "d", "input": 4}])).await;
        let preview = ok(
            &db,
            "POST",
            &format!("/v1/dataset/{did}/restore/preview"),
            json!({"version": v1}),
        )
        .await;
        assert_eq!(preview, json!({"rows_to_restore": 2, "rows_to_delete": 1}));

        let restored = ok(
            &db,
            "POST",
            &format!("/v1/dataset/{did}/restore"),
            json!({"version": v1}),
        )
        .await;
        assert_eq!(
            (
                restored["rows_restored"].clone(),
                restored["rows_deleted"].clone()
            ),
            (json!(2), json!(1))
        );
        assert!(restored["xact_id"].as_str().unwrap() > v1.as_str());
        assert_eq!(contents(&db, &did).await, before);

        // Restoring again changes nothing; history is kept.
        let again = ok(
            &db,
            "POST",
            &format!("/v1/dataset/{did}/restore"),
            json!({"version": v1}),
        )
        .await;
        assert_eq!(
            again,
            json!({"xact_id": null, "rows_restored": 0, "rows_deleted": 0})
        );
        let pinned = restored["xact_id"].as_str().unwrap();
        let old = ok(
            &db,
            "POST",
            &format!("/v1/dataset/{did}/fetch"),
            json!({"version": pinned}),
        )
        .await;
        assert_eq!(old["events"].as_array().unwrap().len(), 3);

        for (uri, body, status) in [
            (
                format!("/v1/dataset/{did}/restore"),
                json!({}),
                StatusCode::BAD_REQUEST,
            ),
            (
                format!("/v1/dataset/{did}/restore"),
                json!({"version": ""}),
                StatusCode::BAD_REQUEST,
            ),
            (
                format!("/v1/dataset/{did}/restore/preview"),
                json!({"version": "nope"}),
                StatusCode::BAD_REQUEST,
            ),
            (
                format!("/v1/dataset/{}/restore", uuid::Uuid::new_v4()),
                json!({"version": v1}),
                StatusCode::FORBIDDEN,
            ),
        ] {
            assert_eq!(call(&db, "POST", &uri, body).await.0, status, "{uri}");
        }
    }
}

#[tokio::test]
async fn dataset_snapshots_through_the_app_endpoints() {
    for (_, db) in common::backends().await {
        let pid = ok(&db, "POST", "/v1/project", json!({"name": "p"})).await["id"].clone();
        let did = ok(
            &db,
            "POST",
            "/v1/dataset",
            json!({"project_id": pid, "name": "d"}),
        )
        .await["id"]
            .clone();
        let register = |name: &str, xact: &str, update: bool| json!({"dataset_id": did, "dataset_snapshot_name": name, "description": "first", "xact_id": xact, "update": update});
        let created = ok(
            &db,
            "POST",
            "/api/dataset_snapshot/register",
            register("v1", "100", false),
        )
        .await;
        assert_eq!(created["found_existing"], false);
        let snap = &created["dataset_snapshot"];
        assert_eq!(
            (
                snap["name"].clone(),
                snap["xact_id"].clone(),
                snap["dataset_id"].clone()
            ),
            (json!("v1"), json!("100"), did.clone())
        );
        let id = snap["id"].as_str().unwrap().to_owned();

        let same = ok(
            &db,
            "POST",
            "/api/dataset_snapshot/register",
            register("v1", "200", false),
        )
        .await;
        assert_eq!(
            (
                same["found_existing"].clone(),
                same["dataset_snapshot"]["xact_id"].clone()
            ),
            (json!(true), json!("100"))
        );
        let moved = ok(
            &db,
            "POST",
            "/api/dataset_snapshot/register",
            register("v1", "200", true),
        )
        .await;
        assert_eq!(
            (
                moved["dataset_snapshot"]["id"].clone(),
                moved["dataset_snapshot"]["xact_id"].clone()
            ),
            (json!(id), json!("200"))
        );
        ok(
            &db,
            "POST",
            "/api/dataset_snapshot/register",
            register("v2", "300", false),
        )
        .await;

        let get = |filter: Value| {
            let db = db.clone();
            async move {
                ok(&db, "POST", "/api/dataset_snapshot/get", filter)
                    .await
                    .as_array()
                    .unwrap()
                    .len()
            }
        };
        assert_eq!(get(json!({"dataset_id": did})).await, 2);
        assert_eq!(get(json!({"dataset_id": did, "name": "v1"})).await, 1);
        assert_eq!(get(json!({"dataset_id": did, "xact_id": "300"})).await, 1);

        let patched = ok(
            &db,
            "POST",
            "/api/dataset_snapshot/patch_id",
            json!({"id": id, "name": "renamed", "description": null}),
        )
        .await;
        assert_eq!(
            (patched["name"].clone(), patched["description"].clone()),
            (json!("renamed"), json!("first"))
        );
        // Names are unique per dataset; keeping one's own name is fine.
        let (status, out) = call(
            &db,
            "POST",
            "/api/dataset_snapshot/patch_id",
            json!({"id": id, "name": "v2"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{out}");
        ok(
            &db,
            "POST",
            "/api/dataset_snapshot/patch_id",
            json!({"id": id, "name": "renamed"}),
        )
        .await;
        let described = ok(
            &db,
            "POST",
            "/api/dataset_snapshot/patch_id",
            json!({"id": id, "description": "second"}),
        )
        .await;
        assert_eq!(
            (described["name"].clone(), described["description"].clone()),
            (json!("renamed"), json!("second"))
        );
        assert_eq!(
            ok(
                &db,
                "POST",
                "/api/dataset_snapshot/delete_id",
                json!({"id": id})
            )
            .await["id"],
            id.as_str()
        );
        assert_eq!(
            call(
                &db,
                "POST",
                "/api/dataset_snapshot/delete_id",
                json!({"id": id})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );

        let missing = uuid::Uuid::new_v4().to_string();
        for (body, status) in [
            (
                json!({"dataset_id": missing, "dataset_snapshot_name": "x", "xact_id": "1"}),
                StatusCode::FORBIDDEN,
            ),
            (
                json!({"dataset_id": did, "xact_id": "1"}),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({"dataset_id": did, "dataset_snapshot_name": "x"}),
                StatusCode::BAD_REQUEST,
            ),
        ] {
            assert_eq!(
                call(&db, "POST", "/api/dataset_snapshot/register", body)
                    .await
                    .0,
                status
            );
        }
    }
}

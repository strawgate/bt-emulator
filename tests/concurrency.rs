//! Many SDK clients at once: concurrent get-or-create, row writes, function
//! versions and attachment registrations must never fail or lose data. Runs on a
//! file-backed SQLite database (several connections, WAL) and on Postgres.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const CLIENTS: usize = 24;

async fn call(db: &bt_emulator::db::Db, method: &str, uri: &str, body: Value) -> Value {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", "Bearer t")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = bt_emulator::api::router(db.clone())
        .oneshot(req)
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert_eq!(status, StatusCode::OK, "{method} {uri}: {text}");
    serde_json::from_str(&text).unwrap()
}

async fn databases() -> Vec<bt_emulator::db::Db> {
    let path = std::env::temp_dir().join(format!("bt-emu-concurrency-{}.db", uuid::Uuid::new_v4()));
    let mut dbs = vec![
        bt_emulator::db::Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap(),
    ];
    dbs.extend(
        common::backends()
            .await
            .into_iter()
            .skip(1)
            .map(|(_, db)| db),
    );
    dbs
}

/// Run `CLIENTS` copies of `task` at once and collect their results.
async fn together<F, Fut>(task: F) -> Vec<Value>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = Value> + Send + 'static,
{
    let handles: Vec<_> = (0..CLIENTS).map(|i| tokio::spawn(task(i))).collect();
    let mut out = Vec::new();
    for handle in handles {
        out.push(handle.await.unwrap());
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_clients_agree() {
    for db in databases().await {
        // Every client registers the same project and experiment by name.
        let projects = together(|_| {
            let db = db.clone();
            async move { call(&db, "POST", "/v1/project", json!({"name": "shared"})).await }
        })
        .await;
        let pid = projects[0]["id"].clone();
        assert!(projects.iter().all(|p| p["id"] == pid), "one project");
        let experiments = together(|_| {
            let (db, pid) = (db.clone(), pid.clone());
            async move {
                let body = json!({"project_id": pid, "name": "run", "ensure_new": false});
                call(&db, "POST", "/v1/experiment", body).await
            }
        })
        .await;
        let eid = experiments[0]["id"].as_str().unwrap().to_owned();
        assert!(
            experiments.iter().all(|e| e["id"] == eid.as_str()),
            "one experiment"
        );

        // Every client logs its own rows, and merges into one shared row.
        together(|i| {
            let (db, eid) = (db.clone(), eid.clone());
            async move {
                let rows = json!([
                    {"id": format!("own-{i}"), "experiment_id": eid, "input": i},
                    {"id": "shared", "experiment_id": eid, "_is_merge": true, "metadata": {format!("k{i}"): i}},
                ]);
                call(&db, "POST", "/logs3", json!({"rows": rows, "api_version": 2})).await
            }
        })
        .await;
        let events = call(
            &db,
            "POST",
            &format!("/v1/experiment/{eid}/fetch"),
            json!({}),
        )
        .await["events"]
            .clone();
        assert_eq!(events.as_array().unwrap().len(), CLIENTS + 1);
        let shared = events
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == "shared")
            .unwrap();
        assert_eq!(
            shared["metadata"].as_object().unwrap().len(),
            CLIENTS,
            "no merge lost"
        );

        // Every client saves a version of the same prompt: one function, all versions.
        let saved = together(|i| {
            let (db, pid) = (db.clone(), pid.clone());
            async move {
                let body = json!({"project_id": pid, "name": "p", "slug": "p", "description": format!("v{i}")});
                call(&db, "PUT", "/v1/prompt", body).await
            }
        })
        .await;
        assert!(
            saved.iter().all(|s| s["id"] == saved[0]["id"]),
            "one function id"
        );
        let mut versions: Vec<_> = saved
            .iter()
            .map(|s| s["_xact_id"].as_str().unwrap().to_owned())
            .collect();
        versions.sort();
        versions.dedup();
        assert_eq!(versions.len(), CLIENTS, "every save is its own version");

        // Every client registers the same attachment key: one signed URL.
        let urls = together(|_| {
            let db = db.clone();
            async move {
                let body = json!({"key": "k", "filename": "f", "content_type": "text/plain", "org_id": "o"});
                call(&db, "POST", "/attachment", body).await
            }
        })
        .await;
        assert!(urls.iter().all(|u| u["signedUrl"] == urls[0]["signedUrl"]));
    }
}

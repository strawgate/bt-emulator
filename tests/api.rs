//! Endpoint behavior tests, run against every backend.

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
            .header("host", "emu.test");
        let req = match body {
            Some(b) => req.body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        };
        let resp = bt_emulator::api::router(self.db.clone())
            .oneshot(req.unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes))),
        )
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, body) = self.call(method, path, body).await;
        assert_eq!(status, StatusCode::OK, "{method} {path}: {body}");
        body
    }

    async fn status(&self, method: &str, path: &str, body: Option<Value>) -> StatusCode {
        self.call(method, path, body).await.0
    }

    async fn project(&self, name: &str) -> String {
        self.ok("POST", "/v1/project", Some(json!({"name": name})))
            .await["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn experiment(&self, project_id: &str, name: &str) -> String {
        let body = json!({"project_id": project_id, "name": name});
        self.ok("POST", "/v1/experiment", Some(body)).await["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn dataset(&self, project_id: &str, name: &str) -> String {
        let body = json!({"project_id": project_id, "name": name});
        self.ok("POST", "/v1/dataset", Some(body)).await["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn logs3(&self, rows: Value) -> Value {
        self.ok(
            "POST",
            "/logs3",
            Some(json!({"rows": rows, "api_version": 2})),
        )
        .await
    }

    async fn fetch(&self, ty: &str, id: &str, body: Value) -> Value {
        self.ok("POST", &format!("/v1/{ty}/{id}/fetch"), Some(body))
            .await
    }
}

/// POST and return the `x-bt-cursor` header and the raw body text.
async fn raw(app: &App, path: &str, body: Value) -> (Option<String>, String) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = bt_emulator::api::router(app.db.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let cursor = resp
        .headers()
        .get("x-bt-cursor")
        .map(|h| h.to_str().unwrap().to_owned());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (cursor, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn apps() -> Vec<App> {
    common::backends()
        .await
        .into_iter()
        .map(|(_, db)| App { db })
        .collect()
}

#[tokio::test]
async fn meta_endpoints() {
    for app in apps().await {
        assert_eq!(app.ok("GET", "/ping", None).await, json!({}));
        let login = app.ok("POST", "/api/apikey/login", None).await;
        assert_eq!(login["org_info"][0]["api_url"], "http://emu.test");
        assert_eq!(login["org_info"][0]["realtime_url"], "ws://emu.test");
        let version = app.ok("GET", "/version", None).await;
        assert!(version["logs3_payload_max_bytes"].as_u64().unwrap() >= 6 * 1024 * 1024);
        assert_eq!(app.call("GET", "/", None).await.1, json!("bt-emulator"));
        let (status, body) = app.call("GET", "/nope", None).await;
        assert_eq!(
            (status, body["error"].as_str().unwrap()),
            (
                StatusCode::NOT_FOUND,
                "bt-emulator does not implement /nope"
            )
        );
    }
}

#[tokio::test]
async fn project_crud() {
    for app in apps().await {
        let id = app.project("p1").await;
        // Get-or-create returns the existing project unchanged.
        let again = app
            .ok(
                "POST",
                "/v1/project",
                Some(json!({"name": "p1", "description": "ignored"})),
            )
            .await;
        assert_eq!(
            (again["id"].as_str(), &again["description"]),
            (Some(id.as_str()), &Value::Null)
        );
        let other = app.project("p2").await;

        let patched = app
            .ok(
                "PATCH",
                &format!("/v1/project/{id}"),
                Some(json!({"description": "d", "id": "x"})),
            )
            .await;
        assert_eq!(
            (patched["description"].as_str(), patched["id"].as_str()),
            (Some("d"), Some(id.as_str()))
        );
        assert_eq!(
            app.ok("GET", &format!("/v1/project/{id}"), None).await["description"],
            "d"
        );
        let rename = app
            .call(
                "PATCH",
                &format!("/v1/project/{id}"),
                Some(json!({"name": "p2"})),
            )
            .await;
        assert_eq!(rename.0, StatusCode::CONFLICT, "{}", rename.1);
        assert_eq!(
            app.status("PATCH", &format!("/v1/project/{id}"), Some(json!([1])))
                .await,
            StatusCode::BAD_REQUEST
        );
        app.ok(
            "PATCH",
            &format!("/v1/project/{id}"),
            Some(json!({"name": "p1"})),
        )
        .await;

        let list = app.ok("GET", "/v1/project?project_name=p1", None).await;
        assert_eq!(list["objects"].as_array().unwrap().len(), 1);
        let list = app
            .ok("GET", &format!("/v1/project?ids={id}&ids={other}"), None)
            .await;
        assert_eq!(list["objects"].as_array().unwrap().len(), 2);

        let deleted = app
            .ok("DELETE", &format!("/v1/project/{other}"), None)
            .await;
        assert!(deleted["deleted_at"].is_string());
        let (status, body) = app.call("GET", &format!("/v1/project/{other}"), None).await;
        assert_eq!(
            (status, body["Code"].as_str()),
            (StatusCode::FORBIDDEN, Some("ForbiddenError"))
        );
        assert_eq!(
            app.status("GET", "/v1/project/not-a-uuid", None).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            app.status("POST", "/v1/project", Some(json!({}))).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            app.status("GET", "/v1/function", None).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            app.status("GET", "/v1/project?limit=x", None).await,
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn list_pagination_is_newest_first() {
    for app in apps().await {
        let pid = app.project("p").await;
        let ids: Vec<String> = futures_sequential(&app, &pid, 4).await;
        let (app, pid) = (&app, &pid);
        let page = |q: String| async move {
            app_ids(app, &format!("/v1/dataset?project_id={pid}&{q}")).await
        };
        assert_eq!(
            page("limit=2".into()).await,
            vec![ids[3].clone(), ids[2].clone()]
        );
        assert_eq!(
            page(format!("starting_after={}&limit=2", ids[2])).await,
            vec![ids[1].clone(), ids[0].clone()]
        );
        assert_eq!(
            page(format!("ending_before={}&limit=1", ids[1])).await,
            vec![ids[2].clone()]
        );
        assert_eq!(
            page(format!("ending_before={}", ids[1])).await,
            vec![ids[3].clone(), ids[2].clone()]
        );
        assert_eq!(page("dataset_name=d1".into()).await, vec![ids[1].clone()]);
        assert_eq!(app_ids(app, "/v1/dataset?project_name=p").await.len(), 4);
        assert!(
            app_ids(app, "/v1/dataset?project_name=missing")
                .await
                .is_empty()
        );
    }
}

async fn futures_sequential(app: &App, pid: &str, n: usize) -> Vec<String> {
    let mut ids = Vec::new();
    for i in 0..n {
        ids.push(app.dataset(pid, &format!("d{i}")).await);
    }
    ids
}

async fn app_ids(app: &App, path: &str) -> Vec<String> {
    let body = app.ok("GET", path, None).await;
    body["objects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn experiment_create_semantics() {
    for app in apps().await {
        let pid = app.project("p").await;
        let first = app.experiment(&pid, "e").await;
        // Without ensure_new, an existing name is reused.
        assert_eq!(app.experiment(&pid, "e").await, first);
        let first_obj = app
            .ok("GET", &format!("/v1/experiment/{first}"), None)
            .await;
        assert_eq!(
            (&first_obj["base_exp_id"], &first_obj["repo_info"]),
            (&Value::Null, &json!({}))
        );
        let body = json!({"project_id": pid, "name": "e", "ensure_new": true, "repo_info": {"commit": "abc"}, "public": true});
        let fresh = app.ok("POST", "/v1/experiment", Some(body)).await;
        let name = fresh["name"].as_str().unwrap();
        assert!(name.starts_with("e-") && name.len() == 10, "{name}");
        assert_eq!(
            (fresh["commit"].as_str(), fresh["public"].as_bool()),
            (Some("abc"), Some(true))
        );
        let unnamed = app
            .ok(
                "POST",
                "/v1/experiment",
                Some(json!({"project_id": pid, "repo_info": {"branch": "main"}})),
            )
            .await;
        assert!(unnamed["name"].as_str().unwrap().starts_with("main-"));
        let missing = json!({"project_id": "7e5a3a7c-2a5c-4d0b-9a8e-0b7e3e3f9999"});
        assert_eq!(
            app.status("POST", "/v1/experiment", Some(missing)).await,
            StatusCode::FORBIDDEN
        );
        let list = app
            .ok(
                "GET",
                &format!("/v1/experiment?project_id={pid}&experiment_name=e"),
                None,
            )
            .await;
        assert_eq!(list["objects"][0]["id"], first.as_str());
    }
}

#[tokio::test]
async fn logs3_merge_replace_delete_and_fetch() {
    for app in apps().await {
        let pid = app.project("p").await;
        let eid = app.experiment(&pid, "e").await;
        let resp = app
            .logs3(json!([
                {"experiment_id": eid, "id": "a", "input": 1, "metrics": {"start": 1}, "tags": ["x"],
                 "span_id": "s", "root_span_id": "s", "span_parents": null, "created": "2026-01-01T00:00:00.123456+00:00"},
                {"experiment_id": eid, "id": "b", "input": 2, "span_parents": ["s"]},
                {"experiment_id": eid, "input": 3},
            ]))
            .await;
        assert_eq!(
            resp["ids"].as_array().unwrap()[..2],
            [json!("a"), json!("b")]
        );
        let xact1: i64 = resp["xact_id"].as_str().unwrap().parse().unwrap();
        app.logs3(json!([
            {"experiment_id": eid, "id": "a", "_is_merge": true, "metrics": {"end": 2}, "tags": ["y"], "created": "ignored",
             "_audit_source": "app", "_audit_metadata": {"who": "me"}},
            {"experiment_id": eid, "id": "b", "output": "replaced"},
            {"experiment_id": eid, "id": resp["ids"][2], "_object_delete": true},
            {"experiment_id": eid, "id": "new", "_is_merge": true, "output": "merge-insert"},
        ]))
        .await;

        let events = app.fetch("experiment", &eid, json!({})).await["events"].clone();
        let by_id = |id: &str| {
            events
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["id"] == id)
                .cloned()
                .unwrap()
        };
        assert_eq!(events.as_array().unwrap().len(), 3);
        let a = by_id("a");
        assert_eq!(a["metrics"], json!({"start": 1, "end": 2}));
        assert_eq!(a["tags"], json!(["x", "y"]));
        assert_eq!(a["created"], "2026-01-01T00:00:00.123Z");
        assert_eq!(
            (a["is_root"].as_bool(), a["project_id"].as_str()),
            (Some(true), Some(pid.as_str()))
        );
        assert_eq!(a["audit_data"][1]["source"], "app");
        assert_eq!(
            a["audit_data"][1]["metadata"],
            json!({"who": "me", "user_id": bt_emulator::api::USER_ID})
        );
        assert_eq!(
            a["span_attributes"]["created_by_user_id"],
            bt_emulator::api::USER_ID
        );
        let b = by_id("b");
        // A replace drops old fields but keeps identity and history.
        assert_eq!(
            (&b["input"], b["output"].as_str()),
            (&Value::Null, Some("replaced"))
        );
        // A replace drops span_parents too, so the row becomes a root.
        assert_eq!(
            (
                b["is_root"].as_bool(),
                b["audit_data"].as_array().unwrap().len()
            ),
            (Some(true), 2)
        );
        assert_eq!(by_id("new")["span_id"], "new");

        // Snapshot read at the first transaction.
        let old = app
            .fetch("experiment", &eid, json!({"version": xact1.to_string()}))
            .await;
        assert_eq!(old["events"].as_array().unwrap().len(), 3);
        let old = app
            .fetch("experiment", &eid, json!({"version": xact1}))
            .await;
        assert!(
            old["events"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["output"].is_null())
        );
    }
}

#[tokio::test]
async fn fetch_pagination_follows_the_cursor_rule() {
    for app in apps().await {
        let pid = app.project("p").await;
        let did = app.dataset(&pid, "d").await;
        let rows: Vec<Value> = (0..5)
            .map(|i| json!({"dataset_id": did, "id": format!("r{i}"), "input": i}))
            .collect();
        app.logs3(json!(rows)).await;
        let mut seen = Vec::new();
        let mut cursor = Value::Null;
        loop {
            let page = app
                .fetch("dataset", &did, json!({"limit": 2, "cursor": cursor}))
                .await;
            let events = page["events"].as_array().unwrap();
            assert_eq!(page.get("cursor").is_some(), !events.is_empty());
            if events.is_empty() {
                break;
            }
            seen.extend(events.iter().map(|e| e["id"].as_str().unwrap().to_owned()));
            cursor = page["cursor"].clone();
        }
        assert_eq!(seen, ["r4", "r3", "r2", "r1", "r0"]);
        let first = &app.fetch("dataset", &did, json!({"limit": 1})).await["events"][0];
        assert_ne!(first["span_id"], first["id"]);
        assert_eq!(first["span_id"], first["root_span_id"]);

        let get = app
            .ok(
                "GET",
                &format!("/v1/dataset/{did}/fetch?limit=10&version=9223372036854775807"),
                None,
            )
            .await;
        assert_eq!(get["events"].as_array().unwrap().len(), 5);
        let (app, did) = (&app, &did);
        let bad = |body: Value| {
            let path = format!("/v1/dataset/{did}/fetch");
            async move { app.status("POST", &path, Some(body)).await }
        };
        assert_eq!(
            bad(json!({"cursor": "nope"})).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(bad(json!({"version": "v1"})).await, StatusCode::BAD_REQUEST);
        assert_eq!(
            app.status(
                "POST",
                &format!("/v1/function/{did}/fetch"),
                Some(json!({}))
            )
            .await,
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn project_logs_rest_insert_and_feedback() {
    for app in apps().await {
        let pid = app.project("p").await;
        let resp = app
            .ok(
                "POST",
                &format!("/v1/project_logs/{pid}/insert"),
                Some(json!({"events": [{"input": "q", "scores": {"s": 0}}]})),
            )
            .await;
        let row_id = resp["row_ids"][0].as_str().unwrap().to_owned();
        app.logs3(json!([{"project_id": pid, "log_id": "g", "id": "direct", "input": "x"}]))
            .await;
        let feedback = json!({"feedback": [
            {"id": row_id, "scores": {"s": 1}, "comment": "nice", "metadata": {"by": "me"}, "source": "app"},
            {"id": "direct", "tags": ["t"]},
        ]});
        assert_eq!(
            app.ok(
                "POST",
                &format!("/v1/project_logs/{pid}/feedback"),
                Some(feedback)
            )
            .await["status"],
            "success"
        );
        // A comment whose target does not exist is kept as its own row.
        app.logs3(json!([{"project_id": pid, "log_id": "g", "comment": {"text": "orphan"}, "origin": {"id": "ghost"}}])).await;

        let events = app.fetch("project_logs", &pid, json!({})).await["events"].clone();
        let row = events
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == row_id.as_str())
            .unwrap();
        assert_eq!(
            (row["scores"]["s"].as_i64(), row["log_id"].as_str()),
            (Some(1), Some("g"))
        );
        assert_eq!(row["comments"][0]["comment"]["text"], "nice");
        assert_eq!(events.as_array().unwrap().len(), 3);

        for (path, body) in [
            (
                format!("/v1/project_logs/{pid}/insert"),
                json!({"events": {}}),
            ),
            (
                format!("/v1/project_logs/{pid}/feedback"),
                json!({"feedback": {}}),
            ),
            (
                format!("/v1/project_logs/{pid}/feedback"),
                json!({"feedback": [{"scores": {}}]}),
            ),
        ] {
            assert_eq!(
                app.status("POST", &path, Some(body)).await,
                StatusCode::BAD_REQUEST,
                "{path}"
            );
        }
    }
}

#[tokio::test]
async fn logs3_rejects_malformed_payloads() {
    for app in apps().await {
        let pid = app.project("p").await;
        let did = app.dataset(&pid, "d").await;
        for body in [
            Value::Null,
            json!({"rows": {"type": "logs3_overflow", "key": "k"}}),
            json!({"rows": [1]}),
            json!({"rows": [{"id": "no-target"}]}),
        ] {
            let (status, resp) = app.call("POST", "/logs3", Some(body.clone())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {resp}");
        }
        let empty = Request::builder()
            .method("POST")
            .uri("/logs3")
            .body(Body::empty())
            .unwrap();
        let resp = bt_emulator::api::router(app.db.clone())
            .oneshot(empty)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            app.status("POST", "/logs3", None).await,
            StatusCode::BAD_REQUEST
        );
        let broken = futures_util::stream::once(async {
            Err::<axum::body::Bytes, _>(std::io::Error::other("reset"))
        });
        let broken = Request::builder()
            .method("POST")
            .uri("/logs3")
            .body(Body::from_stream(broken))
            .unwrap();
        let resp = bt_emulator::api::router(app.db.clone())
            .oneshot(broken)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let garbage = Request::builder()
            .method("POST")
            .uri("/logs3")
            .body(Body::from("{nope"))
            .unwrap();
        let resp = bt_emulator::api::router(app.db.clone())
            .oneshot(garbage)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // Deleting then re-inserting a row resurrects it.
        app.logs3(json!([{"dataset_id": did, "id": "r", "input": 1}]))
            .await;
        app.logs3(json!([{"dataset_id": did, "id": "r", "_object_delete": true}]))
            .await;
        app.logs3(json!([{"dataset_id": did, "id": "r", "_is_merge": true, "input": 2}]))
            .await;
        let events = app.fetch("dataset", &did, json!({})).await["events"].clone();
        assert_eq!(
            (
                events.as_array().unwrap().len(),
                events[0]["input"].as_i64()
            ),
            (1, Some(2))
        );
    }
}

#[tokio::test]
async fn btql_object_queries() {
    for app in apps().await {
        let pid = app.project("p").await;
        let eid = app.experiment(&pid, "e").await;
        app.logs3(json!([
            {"experiment_id": eid, "id": "1", "span_attributes": {"type": "tool"}},
            {"experiment_id": eid, "id": "2", "span_attributes": {"type": "llm"}},
            {"experiment_id": eid, "id": "3", "span_attributes": {"type": "tool"}},
        ]))
        .await;
        let query = |filter: Value, cursor: Value, limit: u64| {
            json!({"query": {
                "select": [{"op": "star"}],
                "from": {"op": "function", "name": {"op": "ident", "name": ["experiment"]}, "args": [{"op": "literal", "value": eid}]},
                "filter": filter, "cursor": cursor, "limit": limit,
            }})
        };
        let tools = json!({"op": "eq", "left": {"op": "ident", "name": ["span_attributes", "type"]}, "right": {"op": "literal", "value": "tool"}});
        let first = app
            .ok("POST", "/btql", Some(query(tools.clone(), Value::Null, 1)))
            .await;
        assert_eq!(first["data"][0]["id"], "3");
        let second = app
            .ok(
                "POST",
                "/btql",
                Some(query(tools.clone(), first["cursor"].clone(), 1)),
            )
            .await;
        assert_eq!(second["data"][0]["id"], "1");
        let third = app
            .ok(
                "POST",
                "/btql",
                Some(query(tools, second["cursor"].clone(), 1)),
            )
            .await;
        assert_eq!(
            (third["data"].as_array().unwrap().len(), third.get("cursor")),
            (0, None)
        );
        let all = app
            .ok("POST", "/btql", Some(query(Value::Null, Value::Null, 10)))
            .await;
        assert_eq!(all["data"].as_array().unwrap().len(), 3);
        assert_eq!(
            all["freshness_state"]["last_processed_xact_id"],
            all["data"][0]["_xact_id"]
        );

        // JSON Lines output (Ruby SDK), and the SQL text form.
        let mut jsonl = query(Value::Null, Value::Null, 2);
        jsonl["fmt"] = json!("jsonl");
        let resp = raw(&app, "/btql", jsonl).await;
        assert!(resp.0.is_some(), "jsonl pages carry x-bt-cursor");
        assert_eq!(resp.1.lines().count(), 2);
        let text = json!({"fmt": "jsonl", "query": format!("SELECT * FROM experiment('{eid}') WHERE span_attributes.type = 'llm'")});
        let resp = raw(&app, "/btql", text).await;
        assert_eq!(serde_json::from_str::<Value>(&resp.1).unwrap()["id"], "2");
        let empty = json!({"fmt": "jsonl", "query": format!("SELECT * FROM experiment('{eid}') WHERE id = 'none'")});
        assert_eq!(raw(&app, "/btql", empty).await, (None, String::new()));

        let prompts = json!({"query": {"from": {"op": "function", "name": {"name": ["project_prompts"]}, "args": [{"value": pid}]}}});
        assert_eq!(
            app.ok("POST", "/btql", Some(prompts)).await["data"],
            json!([])
        );
        let bad_filter = query(json!({"op": "regex"}), Value::Null, 1);
        assert_eq!(
            app.status("POST", "/btql", Some(bad_filter)).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            app.status("POST", "/btql", Some(json!({"query": "select: *"})))
                .await,
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn summaries_compare_against_the_right_experiment() {
    for app in apps().await {
        let pid = app.project("p").await;
        let base = app.experiment(&pid, "base").await;
        let next = app.experiment(&pid, "next").await;
        let other = app.experiment(&pid, "other").await;
        for (eid, score) in [(&base, 0.0), (&next, 1.0), (&other, 0.5)] {
            app.logs3(
                json!([{"experiment_id": eid, "id": "r", "input": 1, "scores": {"acc": score}}]),
            )
            .await;
        }
        // Default: the most recent earlier experiment in the project.
        let s = app
            .ok("GET", &format!("/v1/experiment/{next}/summarize"), None)
            .await;
        assert_eq!(
            (
                s["comparison_experiment_name"].as_str(),
                s["scores"]["acc"]["diff"].as_f64()
            ),
            (Some("base"), Some(1.0))
        );
        assert_eq!(
            s["experiment_url"],
            "http://emu.test/app/bt-emulator/p/p/experiments/next"
        );
        // Explicit comparison.
        let s = app
            .ok(
                "GET",
                &format!("/v1/experiment/{other}/summarize?comparison_experiment_id={base}"),
                None,
            )
            .await;
        assert_eq!(s["scores"]["acc"]["diff"].as_f64(), Some(0.5));
        // A stored base_exp_id wins over recency.
        app.ok(
            "PATCH",
            &format!("/v1/experiment/{other}"),
            Some(json!({"base_exp_id": base})),
        )
        .await;
        let s = app
            .ok(
                "GET",
                &format!("/v1/experiment/{other}/summarize?summarize_scores=false"),
                None,
            )
            .await;
        assert_eq!(
            (s["comparison_experiment_name"].as_str(), s.get("scores")),
            (Some("base"), None)
        );
        // A base experiment that was deleted falls back to recency.
        let gone = app.experiment(&pid, "gone").await;
        let late = app.experiment(&pid, "late").await;
        app.ok(
            "PATCH",
            &format!("/v1/experiment/{late}"),
            Some(json!({"base_exp_id": gone})),
        )
        .await;
        app.ok("DELETE", &format!("/v1/experiment/{gone}"), None)
            .await;
        let s = app
            .ok("GET", &format!("/v1/experiment/{late}/summarize"), None)
            .await;
        assert_eq!(s["comparison_experiment_name"], "other");
        // The first experiment has nothing to compare against.
        let s = app
            .ok("GET", &format!("/v1/experiment/{base}/summarize"), None)
            .await;
        assert_eq!(
            (
                &s["comparison_experiment_name"],
                s["scores"]["acc"].get("diff")
            ),
            (&Value::Null, None)
        );

        let c = app
            .ok(
                "GET",
                &format!("/experiment-comparison2?experiment_id={next}&base_experiment_id={other}"),
                None,
            )
            .await;
        assert_eq!(c["scores"]["acc"]["diff"].as_f64(), Some(0.5));
        assert_eq!(
            app.status("GET", "/experiment-comparison2", None).await,
            StatusCode::BAD_REQUEST
        );

        let did = app.dataset(&pid, "d").await;
        app.logs3(json!([{"dataset_id": did, "id": "x"}, {"dataset_id": did, "id": "y"}]))
            .await;
        let s = app
            .ok(
                "GET",
                &format!("/v1/dataset/{did}/summarize?summarize_data=true"),
                None,
            )
            .await;
        assert_eq!(
            (s["data_summary"].clone(), s["dataset_name"].as_str()),
            (json!({"total_records": 2}), Some("d"))
        );
        assert_eq!(
            app.ok("GET", &format!("/dataset-summary?dataset_id={did}"), None)
                .await,
            json!({"total_records": 2})
        );
        assert_eq!(
            app.status("GET", "/dataset-summary", None).await,
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn legacy_app_endpoints_used_by_the_js_sdk() {
    for app in apps().await {
        let reg = app
            .ok(
                "POST",
                "/api/project/register",
                Some(json!({"project_name": "lp", "org_id": "x"})),
            )
            .await;
        let pid = reg["project"]["id"].as_str().unwrap().to_owned();
        let by_id = app
            .ok(
                "POST",
                "/api/project/register",
                Some(json!({"project_id": pid})),
            )
            .await;
        assert_eq!(by_id["project"]["name"], "lp");
        let q = app.ok("GET", &format!("/api/project?id={pid}"), None).await;
        assert_eq!(
            (q["name"].as_str(), q["project"]["id"].as_str()),
            (Some("lp"), Some(pid.as_str()))
        );
        assert_eq!(
            app.ok("POST", "/api/project/get", Some(json!({"id": pid})))
                .await[0]["name"],
            "lp"
        );
        assert_eq!(
            app.status("GET", "/api/project", None).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            app.status("POST", "/api/project/get", Some(json!({})))
                .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            app.status("POST", "/api/project/register", Some(json!({})))
                .await,
            StatusCode::BAD_REQUEST
        );

        let register =
            |body: Value| async { app.ok("POST", "/api/experiment/register", Some(body)).await };
        let base = register(json!({"project_name": "lp", "experiment_name": "base"})).await;
        let base_id = base["experiment"]["id"].as_str().unwrap().to_owned();
        let deduped = register(json!({"project_id": pid, "experiment_name": "base"})).await;
        assert_ne!(deduped["experiment"]["id"], base_id.as_str());
        let reused =
            register(json!({"project_id": pid, "experiment_name": "base", "update": true})).await;
        assert_eq!(reused["experiment"]["id"], base_id.as_str());
        let next = register(
            json!({"project_id": pid, "experiment_name": "next", "base_experiment": "base"}),
        )
        .await;
        assert_eq!(next["experiment"]["base_exp_id"], base_id.as_str());
        let (status, body) = app
            .call(
                "POST",
                "/api/experiment/register",
                Some(json!({"project_id": pid, "base_experiment": "nope"})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("base experiment"));

        let got = app
            .ok(
                "POST",
                "/api/experiment/get",
                Some(json!({"project_name": "lp", "experiment_name": "next"})),
            )
            .await;
        assert_eq!(got[0]["name"], "next");
        let none = app
            .ok(
                "POST",
                "/api/experiment/get",
                Some(json!({"project_id": pid, "experiment_name": "zzz"})),
            )
            .await;
        assert_eq!(none, json!([]));
        assert_eq!(
            app.status(
                "POST",
                "/api/experiment/get",
                Some(json!({"project_id": pid}))
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        let unknown = json!({"project_name": "missing", "experiment_name": "x"});
        assert_eq!(
            app.status("POST", "/api/experiment/get", Some(unknown))
                .await,
            StatusCode::BAD_REQUEST
        );

        let next_id = next["experiment"]["id"].as_str().unwrap();
        let b = app
            .ok(
                "POST",
                "/api/base_experiment/get_id",
                Some(json!({"id": next_id})),
            )
            .await;
        assert_eq!(
            (b["base_exp_id"].as_str(), b["base_exp_name"].as_str()),
            (Some(base_id.as_str()), Some("base"))
        );
        let first = app
            .ok(
                "POST",
                "/api/experiment/register",
                Some(json!({"project_name": "solo", "experiment_name": "only"})),
            )
            .await;
        let only = json!({"id": first["experiment"]["id"]});
        assert_eq!(
            app.status("POST", "/api/base_experiment/get_id", Some(only))
                .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            app.status("POST", "/api/base_experiment/get_id", Some(json!({})))
                .await,
            StatusCode::BAD_REQUEST
        );

        let ds = json!({"project_name": "lp", "dataset_name": "golden"});
        let d1 = app
            .ok("POST", "/api/dataset/register", Some(ds.clone()))
            .await;
        let d2 = app.ok("POST", "/api/dataset/register", Some(ds)).await;
        assert_eq!(
            (
                d1["found_existing"].as_bool(),
                d2["found_existing"].as_bool()
            ),
            (Some(false), Some(true))
        );
        assert_eq!(d1["dataset"]["url_slug"], "golden");
        assert_eq!(
            app.status(
                "POST",
                "/api/dataset/register",
                Some(json!({"project_id": pid}))
            )
            .await,
            StatusCode::BAD_REQUEST
        );

        let did = d1["dataset"]["id"].clone();
        let info = app
            .ok(
                "POST",
                "/api/self/get_object_info",
                Some(json!({"object_type": "dataset", "object_ids": [did, "ghost"]})),
            )
            .await;
        assert_eq!(
            info,
            json!([{"object_id": did, "object_name": "golden", "parent_cols": {"project": {"id": pid, "name": "lp"}}}])
        );
        let exp_info = json!({"object_type": "experiment", "object_ids": [base_id]});
        assert_eq!(
            app.ok("POST", "/api/self/get_object_info", Some(exp_info))
                .await[0]["object_name"],
            "base"
        );
        assert_eq!(
            app.status(
                "POST",
                "/api/self/get_object_info",
                Some(json!({"object_type": "x"}))
            )
            .await,
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn database_failures_are_500s() {
    for app in apps().await {
        let pid = app.project("p").await;
        app.db.pool().close().await;
        let (status, body) = app.call("GET", "/v1/project", None).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        let rows = json!({"rows": [{"project_id": pid, "log_id": "g"}]});
        assert_eq!(
            app.status("POST", "/logs3", Some(rows)).await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}

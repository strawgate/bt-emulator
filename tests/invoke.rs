//! Function invocation: prompts against a stub OpenAI-compatible server, inline code
//! in local node/python processes. Run against every backend.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use bt_emulator::invoke::Runtime;
use common::{Llm, text_reply, tool_reply};

struct App {
    db: bt_emulator::db::Db,
    runtime: Runtime,
}

impl App {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, String) {
        let config = bt_emulator::api::Config {
            runtime: self.runtime.clone(),
            ..Default::default()
        };
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", "Bearer t");
        let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
        let resp = bt_emulator::api::app(self.db.clone(), config)
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn json(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let (status, text) = self.call(method, path, Some(body)).await;
        (status, serde_json::from_str(&text).unwrap())
    }

    async fn ok(&self, method: &str, path: &str, body: Value) -> Value {
        let (status, body) = self.json(method, path, body).await;
        assert_eq!(status, StatusCode::OK, "{method} {path}: {body}");
        body
    }

    async fn function(&self, project_id: &str, slug: &str, fields: Value) -> String {
        let mut body = json!({"project_id": project_id, "name": slug, "slug": slug});
        body.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        self.ok("POST", "/v1/function", body).await["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn invoke(&self, id: &str, body: Value) -> (StatusCode, Value) {
        self.json("POST", &format!("/v1/function/{id}/invoke"), body)
            .await
    }

    async fn error(&self, id: &str, body: Value, status: StatusCode) -> String {
        let (got, body) = self.invoke(id, body).await;
        assert_eq!(got, status, "{body}");
        body["error"].as_str().unwrap().to_owned()
    }
}

async fn apps(runtime: &Runtime) -> Vec<App> {
    common::backends()
        .await
        .into_iter()
        .map(|(_, db)| App {
            db,
            runtime: runtime.clone(),
        })
        .collect()
}

fn prompt(messages: Value, params: Value) -> Value {
    json!({"function_data": {"type": "prompt"}, "prompt_data": {
        "options": {"model": "gpt-4o-mini", "params": params},
        "prompt": {"type": "chat", "messages": messages},
    }})
}

#[tokio::test]
async fn prompts_run_against_the_configured_model() {
    let llm = Llm::default();
    let mut runtime = Runtime::default();
    runtime.llm_base_url = Some(llm.start().await);
    runtime.llm_api_key = Some("sk-local".into());
    for app in apps(&runtime).await {
        let pid = app.ok("POST", "/v1/project", json!({"name": "p"})).await["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let greet = app
            .function(
                &pid,
                "greet",
                prompt(
                    json!([{"role": "user", "content": "Say hello to {{input}}"}]),
                    json!({"temperature": 0, "use_cache": true}),
                ),
            )
            .await;
        llm.reply(200, text_reply("Hello, world!").to_string());
        let (status, out) = app.invoke(&greet, json!({"input": "world"})).await;
        assert_eq!((status, out), (StatusCode::OK, json!("Hello, world!")));
        let (headers, request) = llm.last();
        assert_eq!(headers["authorization"], "Bearer sk-local");
        assert_eq!(
            request,
            json!({"model": "gpt-4o-mini", "temperature": 0,
                "messages": [{"role": "user", "content": "Say hello to world"}]})
        );

        // A JSON response format yields the parsed object; extra messages are appended.
        let answer = app
            .function(
                &pid,
                "answer",
                prompt(
                    json!([{"role": "user", "content": "Q: {{input.question}}"}]),
                    json!({"response_format": {"type": "json_object"}}),
                ),
            )
            .await;
        llm.reply(200, text_reply(r#"{"answer": "4"}"#).to_string());
        let extra = json!({"role": "user", "content": "be brief"});
        let (_, out) = app
            .invoke(
                &answer,
                json!({"input": {"question": "2+2?"}, "messages": [extra]}),
            )
            .await;
        assert_eq!(out, json!({"answer": "4"}));
        assert_eq!(llm.last().1["messages"][0]["content"], "Q: 2+2?");
        assert_eq!(llm.last().1["messages"][1], extra);

        // An LLM classifier scores through the select_choice tool.
        let mut scorer = prompt(
            json!([{"role": "user", "content": "Does {{output}} match {{expected}}?"}]),
            json!({}),
        );
        scorer["prompt_data"]["parser"] = json!({"type": "llm_classifier", "use_cot": true, "choice_scores": {"correct": 1, "incorrect": 0}});
        let scorer = app.function(&pid, "scorer", scorer).await;
        llm.reply(
            200,
            tool_reply(json!({"choice": "correct", "reasoning": "same"})).to_string(),
        );
        let (_, out) = app
            .invoke(
                &scorer,
                json!({"input": {"input": "a", "output": "A", "expected": "A"}}),
            )
            .await;
        assert_eq!(
            out,
            json!({"name": "scorer", "score": 1, "metadata": {"choice": "correct", "rationale": "same"}})
        );
        assert_eq!(llm.last().1["messages"][0]["content"], "Does A match A?");

        // Streaming uses Braintrust's SSE events.
        llm.reply(200, text_reply("hi").to_string());
        let (status, sse) = app
            .call(
                "POST",
                &format!("/v1/function/{greet}/invoke"),
                Some(json!({"input": "x", "stream": true})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            sse,
            "event: start\ndata: \n\nevent: text_delta\ndata: \"hi\"\n\nevent: done\ndata: \n\n"
        );
        llm.reply(200, text_reply(r#"{"a": 1}"#).to_string());
        let (_, sse) = app
            .call(
                "POST",
                &format!("/v1/function/{answer}/invoke"),
                Some(json!({"input": {}, "stream": true})),
            )
            .await;
        assert!(
            sse.contains("event: json_delta\ndata: {\"a\":1}\n\n"),
            "{sse}"
        );

        // The SDKs' /function/invoke names the function by id or by project and slug.
        let (_, out) = app
            .json(
                "POST",
                "/function/invoke",
                json!({"function_id": greet, "input": "a"}),
            )
            .await;
        assert_eq!(out, json!("hello"));
        for by in [json!({"project_name": "p"}), json!({"project_id": pid})] {
            let mut body = json!({"slug": "greet", "input": "a"});
            body.as_object_mut()
                .unwrap()
                .extend(by.as_object().unwrap().clone());
            assert_eq!(
                app.ok("POST", "/function/invoke", body).await,
                json!("hello")
            );
        }
        for (body, needle) in [
            (
                json!({"slug": "greet", "project_name": "nope"}),
                "project \"nope\" not found",
            ),
            (
                json!({"slug": "nope", "project_name": "p"}),
                "no function with slug",
            ),
            (json!({"slug": "greet"}), "needs `project_name`"),
            (json!({"global_function": "Factuality"}), "global functions"),
            (json!({"input": 1}), "name the function"),
        ] {
            let (status, out) = app.json("POST", "/function/invoke", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(out["error"].as_str().unwrap().contains(needle), "{out}");
        }

        // A pinned version runs that version.
        let v1 = app
            .ok("GET", &format!("/v1/function/{greet}"), Value::Null)
            .await["_xact_id"]
            .clone();
        app.function(
            &pid,
            "greet",
            prompt(json!([{"role": "user", "content": "v2"}]), json!({})),
        )
        .await;
        app.invoke(&greet, json!({"input": "old", "version": v1}))
            .await;
        assert_eq!(llm.last().1["messages"][0]["content"], "Say hello to old");

        // Model failures are 502s, as from the proxy.
        llm.reply(503, "overloaded");
        assert!(
            app.error(&greet, json!({}), StatusCode::BAD_GATEWAY)
                .await
                .contains("503")
        );
        llm.reply(200, "not json");
        assert!(
            app.error(&greet, json!({}), StatusCode::BAD_GATEWAY)
                .await
                .contains("invalid JSON")
        );
        llm.reply(200, text_reply("no tool call").to_string());
        assert!(
            app.error(&scorer, json!({"input": {}}), StatusCode::BAD_GATEWAY)
                .await
                .contains("select_choice")
        );

        let mut with_tools = prompt(json!([]), json!({}));
        with_tools["prompt_data"]["tool_functions"] = json!([{"type": "function", "id": greet}]);
        let with_tools = app.function(&pid, "with-tools", with_tools).await;
        assert!(
            app.error(&with_tools, json!({}), StatusCode::BAD_REQUEST)
                .await
                .contains("tool_functions")
        );

        // A stored template that does not parse is the caller's to fix, not a 500.
        let broken = app
            .function(
                &pid,
                "broken",
                prompt(json!([{"role": "user", "content": "{{#open}}"}]), json!({})),
            )
            .await;
        assert!(
            app.error(&broken, json!({}), StatusCode::BAD_REQUEST)
                .await
                .contains("invalid prompt template")
        );

        let missing = uuid::Uuid::new_v4().to_string();
        let (status, out) = app.invoke(&missing, json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            out["Message"],
            "Function does not exist or you do not have access"
        );
    }
}

#[tokio::test]
async fn prompts_need_a_reachable_model() {
    let mut runtime = Runtime::default();
    let app = apps(&runtime).await.remove(0);
    let pid = app.ok("POST", "/v1/project", json!({"name": "p"})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = app.function(&pid, "f", prompt(json!([]), json!({}))).await;
    assert!(
        app.error(&id, json!({}), StatusCode::BAD_REQUEST)
            .await
            .contains("--llm-base-url")
    );

    // Nothing listens on port 9 (discard).
    runtime.llm_base_url = Some("http://127.0.0.1:9/v1".into());
    let app = App { runtime, ..app };
    assert!(
        app.error(&id, json!({}), StatusCode::BAD_GATEWAY)
            .await
            .contains("model request failed")
    );
}

fn inline(runtime: &str, code: &str) -> Value {
    json!({"function_data": {"type": "code", "data": {
        "type": "inline", "runtime_context": {"runtime": runtime, "version": "x"}, "code": code,
    }}})
}

#[tokio::test]
async fn inline_code_runs_only_when_allowed() {
    let mut runtime = Runtime::default();
    let app = apps(&runtime).await.remove(0);
    let pid = app.ok("POST", "/v1/project", json!({"name": "p"})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let node = "function handler({ output, expected }, { metadata }) { return { score: output === expected ? 1 : 0, m: metadata }; }";
    let id = app.function(&pid, "node", inline("node", node)).await;
    let body = json!({"input": {"output": "A", "expected": "A"}, "metadata": {"k": 1}});
    assert!(
        app.error(&id, body.clone(), StatusCode::BAD_REQUEST)
            .await
            .contains("--allow-code-execution")
    );

    runtime.allow_code = true;
    runtime.code_timeout = Duration::from_secs(1);
    let app = App {
        runtime: runtime.clone(),
        ..app
    };
    let (status, out) = app.invoke(&id, body).await;
    assert_eq!(
        (status, out),
        (StatusCode::OK, json!({"score": 1, "m": {"k": 1}}))
    );

    let py = "def handler(output, expected):\n    return 1.0 if output == expected else 0.0\n";
    let id = app.function(&pid, "py", inline("python", py)).await;
    let (_, out) = app
        .invoke(
            &id,
            json!({"input": {"output": "A", "expected": "B", "input": "x"}}),
        )
        .await;
    assert_eq!(out, json!(0.0));

    let cases = [
        (
            "python",
            "async def handler(x):\n    return x * 2\n",
            json!(21),
            Ok(json!(42)),
        ),
        (
            "python",
            "def handler(**kw):\n    return sorted(kw)\n",
            json!({"b": 1, "a": 2}),
            Ok(json!(["a", "b"])),
        ),
        (
            "node",
            "async function handler(x) { return undefined; }",
            json!(1),
            Ok(Value::Null),
        ),
        (
            "node",
            "function handler() { throw new Error('boom'); }",
            json!(1),
            Err("boom"),
        ),
        (
            "python",
            "def handler(x):\n    raise ValueError('bad')\n",
            json!(1),
            Err("ValueError: bad"),
        ),
        (
            "node",
            "function handler() { process.stdout.write('x'); return 1; }",
            json!(1),
            Err("invalid JSON"),
        ),
        (
            "node",
            "function handler() { while (true) {} }",
            json!(1),
            Err("timed out"),
        ),
    ];
    for (i, (lang, code, input, want)) in cases.into_iter().enumerate() {
        let id = app
            .function(&pid, &format!("c{i}"), inline(lang, code))
            .await;
        let (status, out) = app.invoke(&id, json!({"input": input})).await;
        match want {
            Ok(want) => assert_eq!((status, out), (StatusCode::OK, want), "{code}"),
            Err(needle) => {
                assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{code}: {out}");
                assert!(
                    out["error"].as_str().unwrap().contains(needle),
                    "{code}: {out}"
                );
            }
        }
    }

    for (slug, fields, needle) in [
        (
            "browser",
            inline("browser", "x"),
            "runtime Some(\"browser\")",
        ),
        (
            "bundle",
            json!({"function_data": {"type": "code", "data": {"type": "bundle", "bundle_id": "b",
                "runtime_context": {"runtime": "node", "version": "x"}, "location": {"type": "function", "index": 0}}}}),
            "bundled code",
        ),
        (
            "global",
            json!({"function_data": {"type": "global", "name": "Factuality"}}),
            "cannot run \"global\"",
        ),
    ] {
        let id = app.function(&pid, slug, fields).await;
        assert!(
            app.error(&id, json!({}), StatusCode::BAD_REQUEST)
                .await
                .contains(needle)
        );
    }

    runtime.node = "/nonexistent/node".into();
    let app = App { runtime, ..app };
    let id = app
        .function(&pid, "n2", inline("node", "function handler() {}"))
        .await;
    assert!(
        app.error(&id, json!({}), StatusCode::INTERNAL_SERVER_ERROR)
            .await
            .contains("cannot run code function")
    );
}

#[tokio::test]
async fn invocations_are_logged_under_their_parent() {
    use base64::Engine;
    let llm = Llm::default();
    let mut runtime = Runtime::default();
    runtime.llm_base_url = Some(llm.start().await);
    for app in apps(&runtime).await {
        let pid = app.ok("POST", "/v1/project", json!({"name": "p"})).await["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let eid = app
            .ok(
                "POST",
                "/v1/experiment",
                json!({"project_id": pid, "name": "e"}),
            )
            .await["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let greet = app
            .function(
                &pid,
                "greet",
                prompt(json!([{"role": "user", "content": "hi"}]), json!({})),
            )
            .await;

        // Under a span of an experiment: a child `function` span with the propagated event.
        let parent = json!({"object_type": "experiment", "object_id": eid,
            "row_ids": {"id": "r", "span_id": "s", "root_span_id": "t"},
            "propagated_event": {"span_attributes": {"purpose": "scorer"}}});
        let body = json!({"input": "x", "metadata": {"m": 1}, "tags": ["a"], "parent": parent});
        assert_eq!(app.invoke(&greet, body).await.1, json!("hello"));
        let events = app
            .ok("POST", &format!("/v1/experiment/{eid}/fetch"), json!({}))
            .await["events"]
            .clone();
        let span = &events[0];
        assert_eq!(
            (span["span_parents"].clone(), span["root_span_id"].clone()),
            (json!(["s"]), json!("t"))
        );
        assert_eq!(
            (span["input"].clone(), span["output"].clone()),
            (json!("x"), json!("hello"))
        );
        assert_eq!(
            (span["metadata"].clone(), span["tags"].clone()),
            (json!({"m": 1}), json!(["a"]))
        );
        let attrs = &span["span_attributes"];
        assert_eq!(
            (
                attrs["name"].clone(),
                attrs["type"].clone(),
                attrs["purpose"].clone()
            ),
            (json!("greet"), json!("function"), json!("scorer"))
        );
        assert!(span["metrics"]["end"].as_f64() >= span["metrics"]["start"].as_f64());

        // A logger exported before its project was known names the project; failures
        // are logged with their error.
        let lazy = base64::engine::general_purpose::STANDARD
            .encode(b"\x04\x02\x00{\"compute_object_metadata_args\":{\"project_name\":\"lazy\"}}");
        llm.reply(500, "down");
        let (status, _) = app
            .invoke(&greet, json!({"input": "y", "parent": lazy}))
            .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let lazy_id = app
            .ok("GET", "/v1/project?project_name=lazy", Value::Null)
            .await["objects"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let logs = app
            .ok(
                "POST",
                &format!("/v1/project_logs/{lazy_id}/fetch"),
                json!({}),
            )
            .await["events"]
            .clone();
        assert!(logs[0]["error"].as_str().unwrap().contains("500"), "{logs}");
        assert_eq!(logs[0]["span_parents"], Value::Null);
        assert_eq!(logs[0]["is_root"], true);

        // By project id; playground parents and empty parents are not logged.
        let by_id = json!({"object_type": "project_logs", "object_id": pid});
        app.invoke(&greet, json!({"input": "z", "parent": by_id}))
            .await;
        let logs = app
            .ok("POST", &format!("/v1/project_logs/{pid}/fetch"), json!({}))
            .await["events"]
            .clone();
        assert_eq!(logs.as_array().unwrap().len(), 1);
        for parent in [
            json!({"object_type": "playground_logs", "object_id": "x"}),
            json!(""),
        ] {
            assert_eq!(
                app.invoke(&greet, json!({"parent": parent})).await.0,
                StatusCode::OK
            );
        }

        // A malformed parent, or one naming a missing object, fails before the call runs.
        let before = llm.requests.lock().unwrap().len();
        assert!(
            app.error(
                &greet,
                json!({"parent": "garbage"}),
                StatusCode::BAD_REQUEST
            )
            .await
            .contains("exported span")
        );
        let missing =
            json!({"object_type": "experiment", "object_id": uuid::Uuid::new_v4().to_string()});
        let (status, _) = app.invoke(&greet, json!({"parent": missing})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let missing =
            json!({"object_type": "project_logs", "object_id": uuid::Uuid::new_v4().to_string()});
        assert_eq!(
            app.invoke(&greet, json!({"parent": missing})).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(llm.requests.lock().unwrap().len(), before);
    }
}

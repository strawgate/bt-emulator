#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, StatusCode};
use bt_emulator::db::Db;
use serde_json::{Value, json};

/// Every backend the test suite should exercise.
///
/// SQLite (in-memory) always runs. Postgres runs when `BT_TEST_POSTGRES_URL`
/// points at a server we may create databases on; each call gets a fresh
/// database so tests stay isolated.
pub async fn backends() -> Vec<(&'static str, Db)> {
    let mut out = vec![("sqlite", Db::connect("sqlite::memory:").await.unwrap())];
    if let Ok(admin_url) = std::env::var("BT_TEST_POSTGRES_URL") {
        out.push(("postgres", fresh_postgres(&admin_url).await));
    } else if std::env::var_os("BT_REQUIRE_POSTGRES").is_some() {
        panic!("BT_REQUIRE_POSTGRES is set but BT_TEST_POSTGRES_URL is not");
    }
    out
}

/// Test databases are named `bt_test_<unix secs>_<uuid>`. Nothing drops them when a
/// test ends, so each test process first drops those over an hour old: no test
/// runs that long, so this never touches another run's live database.
const STALE_SECS: i64 = 3600;
static CLEANED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn drop_stale(admin: &Db) {
    use sqlx::Row;
    let now = chrono::Utc::now().timestamp();
    let names: Vec<String> = sqlx::query(
        "SELECT datname::text AS datname FROM pg_database WHERE datname LIKE 'bt_test_%'",
    )
    .fetch_all(admin.pool())
    .await
    .unwrap()
    .iter()
    .map(|row| row.get::<String, _>("datname"))
    .collect();
    for name in names {
        let created = name.split('_').nth(2).and_then(|t| t.parse::<i64>().ok());
        // Names without a timestamp predate this scheme and are always stale.
        if created.is_none_or(|t| now - t > STALE_SECS) {
            let _ = admin
                .execute_raw(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .await;
        }
    }
}

async fn fresh_postgres(admin_url: &str) -> Db {
    let admin = Db::connect_without_migrations(admin_url).await.unwrap();
    CLEANED.get_or_init(|| drop_stale(&admin)).await;
    let name = format!(
        "bt_test_{}_{}",
        chrono::Utc::now().timestamp(),
        uuid::Uuid::new_v4().simple()
    );
    admin
        .execute_raw(&format!("CREATE DATABASE {name}"))
        .await
        .unwrap();
    let base = admin_url.rsplit_once('/').unwrap().0;
    Db::connect(&format!("{base}/{name}")).await.unwrap()
}

/// A stub chat completions server. Each request is recorded; the reply is the next
/// queued `(status, body)`, else the sticky reply.
#[derive(Clone, Default)]
pub struct Llm {
    pub requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    pub replies: Arc<Mutex<Vec<(u16, String)>>>,
    /// The reply once the queue is empty (default: the text "hello").
    pub sticky: Arc<Mutex<Option<String>>>,
}

impl Llm {
    pub async fn start(&self) -> String {
        let me = self.clone();
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move |headers: HeaderMap, body: String| {
                let me = me.clone();
                async move {
                    me.requests
                        .lock()
                        .unwrap()
                        .push((headers, serde_json::from_str(&body).unwrap()));
                    let mut replies = me.replies.lock().unwrap();
                    let (status, body) = if replies.is_empty() {
                        let sticky = me.sticky.lock().unwrap().clone();
                        (
                            200,
                            sticky.unwrap_or_else(|| text_reply("hello").to_string()),
                        )
                    } else {
                        replies.remove(0)
                    };
                    (StatusCode::from_u16(status).unwrap(), body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/v1/")
    }

    pub fn reply(&self, status: u16, body: impl Into<String>) {
        self.replies.lock().unwrap().push((status, body.into()));
    }

    pub fn last(&self) -> (HeaderMap, Value) {
        self.requests.lock().unwrap().last().unwrap().clone()
    }
}

pub fn text_reply(content: &str) -> Value {
    json!({"choices": [{"message": {"role": "assistant", "content": content}}]})
}

pub fn tool_reply(args: Value) -> Value {
    json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [
        {"id": "c", "type": "function", "function": {"name": "select_choice", "arguments": args.to_string()}},
    ]}}]})
}

//! Replays recorded Braintrust API traffic (compat/corpus, harvested from upstream
//! SDK cassettes) against the emulator and checks wire compatibility.
//!
//! For each cassette, on a fresh database per backend, interactions are replayed in
//! order. Server-generated ids in recorded responses are mapped to the ids the
//! emulator returned, and substituted into later requests. Each response must:
//!   * have the recorded status code,
//!   * contain every recorded JSON key with a compatible type,
//!   * echo exactly any string the client previously sent that the recording echoed.
//!
//! Cassettes known to fail are listed in compat/expected-failures.txt. The test fails
//! if that ledger is stale in either direction; regenerate it with
//! `BT_COMPAT_BLESS=1 cargo test --test compat`.

mod common;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use axum::body::Body;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const LEDGER: &str = "compat/expected-failures.txt";

struct Cassette {
    id: String,
    interactions: Vec<Value>,
}

fn load_corpus() -> Vec<Cassette> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("compat/corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .flat_map(|src| std::fs::read_dir(src.unwrap().path()).unwrap())
        .map(|f| f.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let doc: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let id = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let interactions = doc["interactions"].as_array().unwrap().clone();
            Cassette { id, interactions }
        })
        .collect()
}

#[derive(Default)]
struct Replay {
    /// recorded server-generated value -> value the emulator produced
    ids: HashMap<String, String>,
    /// every string the client has sent so far (after substitution)
    sent: HashSet<String>,
}

fn is_id_key(key: &str) -> bool {
    key == "id" || key.ends_with("_id") || key == "_xact_id" || key == "version" || key == "cursor"
}

impl Replay {
    fn substitute(&self, s: &str) -> String {
        let mut out = s.to_owned();
        for (from, to) in &self.ids {
            if out.contains(from.as_str()) {
                out = out.replace(from.as_str(), to);
            }
        }
        out
    }

    fn substitute_value(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.substitute(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.substitute_value(x)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| (k.clone(), self.substitute_value(x)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    fn record_sent(&mut self, v: &Value) {
        match v {
            Value::String(s) => {
                self.sent.insert(s.clone());
            }
            Value::Array(a) => a.iter().for_each(|x| self.record_sent(x)),
            Value::Object(o) => o.values().for_each(|x| self.record_sent(x)),
            _ => {}
        }
    }

    /// Learn id mappings from a recorded/actual response pair.
    fn learn(&mut self, key: Option<&str>, recorded: &Value, actual: &Value) {
        match (recorded, actual) {
            (Value::String(r), Value::String(a))
                if key.is_some_and(is_id_key) && r != a && r.len() >= 4 =>
            {
                self.ids.entry(r.clone()).or_insert_with(|| a.clone());
            }
            (Value::Object(r), Value::Object(a)) => {
                for (k, rv) in r {
                    if let Some(av) = a.get(k) {
                        self.learn(Some(k), rv, av);
                    }
                }
            }
            (Value::Array(r), Value::Array(a)) => {
                for (rv, av) in r.iter().zip(a) {
                    self.learn(key, rv, av);
                }
            }
            _ => {}
        }
    }

    /// Compare shapes; returns the first incompatibility found.
    fn compare(&self, path: &str, recorded: &Value, actual: &Value) -> Result<(), String> {
        match (recorded, actual) {
            (Value::Null, _) => Ok(()),
            (Value::Object(r), Value::Object(a)) => {
                for (k, rv) in r {
                    let Some(av) = a.get(k) else {
                        return Err(format!("{path}.{k}: missing"));
                    };
                    self.compare(&format!("{path}.{k}"), rv, av)?;
                }
                Ok(())
            }
            (Value::Array(r), Value::Array(a)) => match (r.first(), a.first()) {
                // Only data arrays must be non-empty; config lists may legitimately differ.
                (Some(_), None) if is_data_array(path) => {
                    Err(format!("{path}: expected non-empty array"))
                }
                (Some(_), None) => Ok(()),
                (Some(rv), Some(av)) => {
                    // Prefer the element with the same id: row order across concurrent
                    // writes is not part of the contract.
                    let rid = rv["id"].as_str().map(|id| self.substitute(id));
                    let av = rid
                        .and_then(|id| a.iter().find(|x| x["id"] == id.as_str()))
                        .unwrap_or(av);
                    self.compare(&format!("{path}[0]"), rv, av)
                }
                _ => Ok(()),
            },
            (Value::String(r), Value::String(a)) => {
                let r = self.substitute(r);
                if self.sent.contains(&r) && &r != a {
                    Err(format!("{path}: expected echoed {r:?}, got {a:?}"))
                } else {
                    Ok(())
                }
            }
            (Value::Number(_), Value::Number(_)) | (Value::Bool(_), Value::Bool(_)) => Ok(()),
            (r, a) => Err(format!("{path}: expected {}, got {}", kind(r), kind(a))),
        }
    }
}

fn is_data_array(path: &str) -> bool {
    path == "$"
        || ["objects", "events", "data", "ids", "row_ids"]
            .iter()
            .any(|k| path.ends_with(&format!(".{k}")))
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Parse a response body: JSON, else JSON Lines (BTQL `fmt: "jsonl"`), else text.
fn parse_body(text: &str) -> Value {
    if text.trim().is_empty() {
        return Value::Null;
    }
    if let Ok(v) = serde_json::from_str(text) {
        return v;
    }
    let rows: Result<Vec<Value>, _> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect();
    rows.map_or_else(|_| Value::String(text.to_owned()), Value::Array)
}

async fn send(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let req = axum::http::Request::builder().method(method).uri(path);
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    let resp = router
        .clone()
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, parse_body(&String::from_utf8_lossy(&bytes)))
}

/// Recordings were made against a real org, so a list call may return objects created
/// before the recording started. Seed them through the public API so the rest of the
/// cassette can replay. Only names and parents are seeded, never rows.
async fn hydrate(router: &axum::Router, st: &mut Replay, it: &Value) {
    let path = it["path"].as_str().unwrap();
    let Some(kind) = ["project", "experiment", "dataset"]
        .into_iter()
        .find(|k| path.starts_with(&format!("/v1/{k}?")))
    else {
        return;
    };
    if it["method"] != "GET" {
        return;
    }
    // Lists are newest first; create oldest first so the order matches.
    for obj in it["response"]["body"]["objects"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
    {
        let id = obj["id"].as_str().unwrap_or_default();
        if st.ids.contains_key(id) {
            continue;
        }
        let mut body = st.substitute_value(obj);
        for key in ["id", "created", "user_id", "org_id", "deleted_at"] {
            body.as_object_mut().unwrap().remove(key);
        }
        if kind != "project" {
            let Some(pid) = obj["project_id"].as_str().and_then(|p| st.ids.get(p)) else {
                continue;
            };
            body["project_id"] = json!(pid);
        }
        let (status, created) = send(router, "POST", &format!("/v1/{kind}"), Some(body)).await;
        if status == 200 {
            st.learn(None, obj, &created);
        }
    }
}

async fn replay(router: axum::Router, cassette: &Cassette) -> Result<(), String> {
    let mut st = Replay::default();
    for (n, it) in cassette.interactions.iter().enumerate() {
        hydrate(&router, &mut st, it).await;
        let method = it["method"].as_str().unwrap();
        let path = st.substitute(it["path"].as_str().unwrap());
        let where_ = format!("#{n} {method} {}", it["path"].as_str().unwrap());
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(&path)
            .header("authorization", "Bearer bt-emulator-local");
        let mut has_content_type = false;
        if let Some(h) = it["request"]["headers"].as_object() {
            for (k, v) in h {
                if let Some(v) = v.as_str() {
                    has_content_type |= k == "content-type";
                    req = req.header(k, st.substitute(v));
                }
            }
        }
        let body = match &it["request"]["body"] {
            Value::Null => Body::empty(),
            Value::String(s) => Body::from(st.substitute(s)),
            v => {
                let v = st.substitute_value(v);
                st.record_sent(&v);
                if !has_content_type {
                    req = req.header("content-type", "application/json");
                }
                Body::from(serde_json::to_vec(&v).unwrap())
            }
        };
        if let Some(q) = path.split_once('?').map(|(_, q)| q) {
            for pair in q.split('&') {
                let v = pair.split_once('=').map_or(pair, |(_, v)| v);
                st.sent.insert(percent_decode(v));
            }
        }
        let resp = router
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let cursor_header = resp
            .headers()
            .get("x-bt-cursor")
            .map(|h| h.to_str().unwrap().to_owned());
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let expected_status = it["response"]["status"].as_u64().unwrap() as u16;
        if status != expected_status {
            let body = String::from_utf8_lossy(&bytes);
            return Err(format!(
                "{where_}: status {status}, expected {expected_status}: {:.200}",
                body
            ));
        }
        if let Some(recorded) = it["response"]["headers"]["x-bt-cursor"].as_str() {
            let ours =
                cursor_header.ok_or_else(|| format!("{where_}: missing x-bt-cursor header"))?;
            st.ids.entry(recorded.to_owned()).or_insert(ours);
        }
        // BTQL JSON Lines: a single row parses as an object, so normalize to rows.
        let jsonl = it["request"]["body"]["fmt"] == "jsonl";
        let rows = |v: Value| {
            if jsonl && v.is_object() {
                Value::Array(vec![v])
            } else {
                v
            }
        };
        let recorded = rows(match &it["response"]["body"] {
            Value::String(text) => parse_body(text),
            other => other.clone(),
        });
        if recorded.is_object() || recorded.is_array() {
            let actual = rows(parse_body(&String::from_utf8_lossy(&bytes)));
            if !(actual.is_object() || actual.is_array()) {
                return Err(format!("{where_}: response is not JSON: {actual:.200}"));
            }
            st.compare("$", &recorded, &actual)
                .map_err(|e| format!("{where_}: {e}"))?;
            st.learn(None, &recorded, &actual);
        }
    }
    Ok(())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_ledger(path: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.split('#').next().unwrap().trim().to_owned())
        .filter(|l| !l.is_empty())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn recorded_sdk_traffic_replays() {
    let corpus = load_corpus();
    assert!(
        !corpus.is_empty(),
        "compat/corpus is empty; run `just compat-harvest`"
    );
    let mut failures: BTreeMap<String, String> = BTreeMap::new();
    let mut per_backend: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for cassette in &corpus {
        for (backend, db) in common::backends().await {
            if let Err(reason) = replay(bt_emulator::api::router(db), cassette).await {
                per_backend
                    .entry(backend)
                    .or_default()
                    .insert(cassette.id.clone());
                failures.entry(cassette.id.clone()).or_insert(reason);
            }
        }
    }
    // Backend parity: every backend must fail exactly the same cassettes.
    let sets: Vec<_> = per_backend.values().collect();
    if per_backend.len() > 1 {
        assert!(
            sets.windows(2).all(|w| w[0] == w[1]),
            "backends disagree: {per_backend:#?}"
        );
    }

    let ledger_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(LEDGER);
    let total = corpus.len();
    let passed = total - failures.len();
    println!("compat: {passed}/{total} cassettes pass");
    if std::env::var_os("BT_COMPAT_BLESS").is_some() {
        let mut out = format!(
            "# Cassettes in compat/corpus that do not replay yet ({passed}/{total} pass).\n\
             # Regenerate: BT_COMPAT_BLESS=1 cargo test --test compat\n"
        );
        for (id, reason) in &failures {
            let reason: String = reason
                .chars()
                .take(160)
                .collect::<String>()
                .replace('\n', " ");
            out.push_str(&format!("{id}  # {reason}\n"));
        }
        std::fs::write(&ledger_path, out).unwrap();
        return;
    }
    let expected = read_ledger(&ledger_path);
    let actual: BTreeSet<String> = failures.keys().cloned().collect();
    let newly_failing: Vec<_> = actual
        .difference(&expected)
        .map(|id| format!("{id}: {}", failures[id]))
        .collect();
    let newly_passing: Vec<_> = expected.difference(&actual).collect();
    assert!(
        newly_failing.is_empty() && newly_passing.is_empty(),
        "compat ledger is stale.\nNewly failing:\n  {}\nNewly passing (remove from {LEDGER}):\n  {}\n\
         Regenerate with BT_COMPAT_BLESS=1 cargo test --test compat",
        newly_failing.join("\n  "),
        newly_passing
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  "),
    );
}

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
use serde_json::Value;
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
                (Some(_), None) => Err(format!("{path}: expected non-empty array")),
                (Some(rv), Some(av)) => self.compare(&format!("{path}[0]"), rv, av),
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

async fn replay(router: axum::Router, cassette: &Cassette) -> Result<(), String> {
    let mut st = Replay::default();
    for (n, it) in cassette.interactions.iter().enumerate() {
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
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let expected_status = it["response"]["status"].as_u64().unwrap() as u16;
        if status != expected_status {
            let body = String::from_utf8_lossy(&bytes);
            return Err(format!(
                "{where_}: status {status}, expected {expected_status}: {:.200}",
                body
            ));
        }
        let recorded = &it["response"]["body"];
        if recorded.is_object() || recorded.is_array() {
            let actual: Value = serde_json::from_slice(&bytes)
                .map_err(|e| format!("{where_}: response is not JSON ({e})"))?;
            st.compare("$", recorded, &actual)
                .map_err(|e| format!("{where_}: {e}"))?;
            st.learn(None, recorded, &actual);
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

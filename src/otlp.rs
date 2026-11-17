//! OTLP trace ingest (docs/protocol.md §17): decode `ExportTraceServiceRequest`
//! (protobuf or JSON) and map each span to a Braintrust row.

use base64::Engine;
use serde_json::{Map, Value, json};

/// The subset of the OTLP trace protobuf the emulator reads
/// (opentelemetry/proto/trace/v1/trace.proto, common/v1/common.proto).
pub mod proto {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ExportTraceServiceRequest {
        #[prost(message, repeated, tag = "1")]
        pub resource_spans: Vec<ResourceSpans>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ResourceSpans {
        #[prost(message, repeated, tag = "2")]
        pub scope_spans: Vec<ScopeSpans>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ScopeSpans {
        #[prost(message, repeated, tag = "2")]
        pub spans: Vec<Span>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Span {
        #[prost(bytes = "vec", tag = "1")]
        pub trace_id: Vec<u8>,
        #[prost(bytes = "vec", tag = "2")]
        pub span_id: Vec<u8>,
        #[prost(bytes = "vec", tag = "4")]
        pub parent_span_id: Vec<u8>,
        #[prost(string, tag = "5")]
        pub name: String,
        #[prost(fixed64, tag = "7")]
        pub start_time_unix_nano: u64,
        #[prost(fixed64, tag = "8")]
        pub end_time_unix_nano: u64,
        #[prost(message, repeated, tag = "9")]
        pub attributes: Vec<KeyValue>,
        #[prost(message, repeated, tag = "11")]
        pub events: Vec<Event>,
        #[prost(message, optional, tag = "15")]
        pub status: Option<Status>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Event {
        #[prost(string, tag = "2")]
        pub name: String,
        #[prost(message, repeated, tag = "3")]
        pub attributes: Vec<KeyValue>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Status {
        #[prost(string, tag = "2")]
        pub message: String,
        #[prost(int32, tag = "3")]
        pub code: i32,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct KeyValue {
        #[prost(string, tag = "1")]
        pub key: String,
        #[prost(message, optional, tag = "2")]
        pub value: Option<AnyValue>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct AnyValue {
        #[prost(oneof = "any_value::Value", tags = "1, 2, 3, 4, 5, 6, 7")]
        pub value: Option<any_value::Value>,
    }
    pub mod any_value {
        #[derive(Clone, PartialEq, prost::Oneof)]
        pub enum Value {
            #[prost(string, tag = "1")]
            StringValue(String),
            #[prost(bool, tag = "2")]
            BoolValue(bool),
            #[prost(int64, tag = "3")]
            IntValue(i64),
            #[prost(double, tag = "4")]
            DoubleValue(f64),
            #[prost(message, tag = "5")]
            ArrayValue(super::ArrayValue),
            #[prost(message, tag = "6")]
            KvlistValue(super::KeyValueList),
            #[prost(bytes, tag = "7")]
            BytesValue(Vec<u8>),
        }
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ArrayValue {
        #[prost(message, repeated, tag = "1")]
        pub values: Vec<AnyValue>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct KeyValueList {
        #[prost(message, repeated, tag = "1")]
        pub values: Vec<KeyValue>,
    }
}

/// A span, independent of its wire encoding. Ids are lowercase hex; times are
/// nanoseconds since the epoch; attribute values are plain JSON.
#[derive(Debug, Default, PartialEq)]
pub struct Span {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: String,
    pub name: String,
    pub start: u64,
    pub end: u64,
    pub attributes: Map<String, Value>,
    /// `(name, attributes)` of each event.
    pub events: Vec<(String, Map<String, Value>)>,
    /// OTLP status code (2 = error) and message.
    pub status: (i64, String),
}

pub fn decode_protobuf(bytes: &[u8]) -> Result<Vec<Span>, String> {
    use proto::any_value::Value as V;
    fn any(v: Option<proto::AnyValue>) -> Value {
        match v.and_then(|v| v.value) {
            Some(V::StringValue(s)) => json!(s),
            Some(V::BoolValue(b)) => json!(b),
            Some(V::IntValue(i)) => json!(i),
            Some(V::DoubleValue(d)) => json!(d),
            Some(V::ArrayValue(a)) => a.values.into_iter().map(|v| any(Some(v))).collect(),
            Some(V::KvlistValue(kv)) => Value::Object(attrs(kv.values)),
            // Base64, as OTLP/JSON carries bytes, so both encodings store the same.
            Some(V::BytesValue(b)) => json!(base64::engine::general_purpose::STANDARD.encode(b)),
            None => Value::Null,
        }
    }
    fn attrs(kvs: Vec<proto::KeyValue>) -> Map<String, Value> {
        kvs.into_iter().map(|kv| (kv.key, any(kv.value))).collect()
    }
    let request = <proto::ExportTraceServiceRequest as prost::Message>::decode(bytes)
        .map_err(|e| format!("invalid OTLP protobuf: {e}"))?;
    let spans = request
        .resource_spans
        .into_iter()
        .flat_map(|r| r.scope_spans)
        .flat_map(|s| s.spans)
        .map(|s| {
            let status = s.status.unwrap_or_default();
            Span {
                trace_id: hex::encode(s.trace_id),
                span_id: hex::encode(s.span_id),
                parent_span_id: hex::encode(s.parent_span_id),
                name: s.name,
                start: s.start_time_unix_nano,
                end: s.end_time_unix_nano,
                attributes: attrs(s.attributes),
                events: s
                    .events
                    .into_iter()
                    .map(|e| (e.name, attrs(e.attributes)))
                    .collect(),
                status: (i64::from(status.code), status.message),
            }
        })
        .collect();
    Ok(spans)
}

/// OTLP/JSON: lowerCamelCase fields, hex ids, 64-bit integers as strings or numbers.
pub fn decode_json(body: &Value) -> Result<Vec<Span>, String> {
    fn int(v: &Value) -> Option<i64> {
        v.as_i64().or_else(|| v.as_str()?.parse().ok())
    }
    fn any(v: &Value) -> Value {
        if let Some(s) = v.get("stringValue") {
            s.clone()
        } else if let Some(i) = v.get("intValue") {
            int(i).map_or(Value::Null, |i| json!(i))
        } else if let Some(a) = v.get("arrayValue") {
            a["values"]
                .as_array()
                .into_iter()
                .flatten()
                .map(any)
                .collect()
        } else if let Some(kv) = v.get("kvlistValue") {
            Value::Object(attrs(&kv["values"]))
        } else {
            // boolValue, doubleValue and bytesValue (base64) are already JSON.
            ["boolValue", "doubleValue", "bytesValue"]
                .iter()
                .find_map(|k| v.get(*k).cloned())
                .unwrap_or(Value::Null)
        }
    }
    fn attrs(kvs: &Value) -> Map<String, Value> {
        let kvs = kvs.as_array().map(Vec::as_slice).unwrap_or_default();
        kvs.iter()
            .filter_map(|kv| Some((kv["key"].as_str()?.to_owned(), any(&kv["value"]))))
            .collect()
    }
    let text = |v: &Value| v.as_str().unwrap_or_default().to_ascii_lowercase();
    let list = |v: &Value, key: &str| v[key].as_array().cloned().unwrap_or_default();
    if !body.is_object() {
        return Err("OTLP JSON body must be an object".into());
    }
    let mut spans = Vec::new();
    for resource in list(body, "resourceSpans") {
        for scope in list(&resource, "scopeSpans") {
            for s in list(&scope, "spans") {
                let nanos = |k: &str| int(&s[k]).unwrap_or_default() as u64;
                let events = list(&s, "events")
                    .iter()
                    .map(|e| {
                        (
                            e["name"].as_str().unwrap_or_default().to_owned(),
                            attrs(&e["attributes"]),
                        )
                    })
                    .collect();
                spans.push(Span {
                    trace_id: text(&s["traceId"]),
                    span_id: text(&s["spanId"]),
                    parent_span_id: text(&s["parentSpanId"]),
                    name: s["name"].as_str().unwrap_or_default().to_owned(),
                    start: nanos("startTimeUnixNano"),
                    end: nanos("endTimeUnixNano"),
                    attributes: attrs(&s["attributes"]),
                    events,
                    status: (
                        int(&s["status"]["code"]).unwrap_or_default(),
                        s["status"]["message"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                    ),
                });
            }
        }
    }
    Ok(spans)
}

/// A JSON-encoded attribute, or the value itself when it is not a string of JSON.
fn parsed(v: &Value) -> Value {
    v.as_str()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| v.clone())
}

/// `braintrust.<name>` as a JSON object (or a kvlist), merged with any flattened
/// `braintrust.<name>.<key>` attributes.
fn object(attrs: &Map<String, Value>, name: &str) -> Map<String, Value> {
    let mut out = match attrs.get(&format!("braintrust.{name}")).map(parsed) {
        Some(Value::Object(o)) => o,
        _ => Map::new(),
    };
    let prefix = format!("braintrust.{name}.");
    for (key, value) in attrs {
        if let Some(field) = key.strip_prefix(&prefix) {
            out.insert(field.to_owned(), value.clone());
        }
    }
    out
}

/// The first attribute present among `keys`, decoded.
fn first(attrs: &Map<String, Value>, keys: &[&str]) -> Option<Value> {
    keys.iter().find_map(|k| attrs.get(*k)).map(parsed)
}

/// A row for `span`, without its destination (set by the caller from `parent`).
/// Returns the span's `braintrust.parent` attribute, if any, alongside.
pub fn to_row(span: &Span) -> (Option<String>, Map<String, Value>) {
    let a = &span.attributes;
    let mut row = Map::new();
    row.insert("id".into(), json!(span.span_id));
    row.insert("span_id".into(), json!(span.span_id));
    row.insert("root_span_id".into(), json!(span.trace_id));
    if !span.parent_span_id.is_empty() {
        row.insert("span_parents".into(), json!([span.parent_span_id]));
    }
    let start = chrono::DateTime::from_timestamp_nanos(span.start as i64);
    row.insert("created".into(), json!(crate::time::format_iso(start)));
    let fields: [(&str, &[&str]); 4] = [
        (
            "input",
            &[
                "braintrust.input_json",
                "braintrust.input",
                "gen_ai.input.messages",
                "gen_ai.prompt_json",
                "gen_ai.prompt",
            ],
        ),
        (
            "output",
            &[
                "braintrust.output_json",
                "braintrust.output",
                "gen_ai.output.messages",
                "gen_ai.completion_json",
                "gen_ai.completion",
            ],
        ),
        (
            "expected",
            &["braintrust.expected_json", "braintrust.expected"],
        ),
        (
            "context",
            &["braintrust.context_json", "braintrust.context"],
        ),
    ];
    for (field, keys) in fields {
        if let Some(value) = first(a, keys) {
            row.insert(field.into(), value);
        }
    }
    if let Some(tags) = a.get("braintrust.tags").map(parsed) {
        row.insert("tags".into(), tags);
    }

    let mut metadata = object(a, "metadata");
    if let Some(model) = a.get("gen_ai.request.model") {
        metadata.entry("model").or_insert(model.clone());
    }
    let mut metrics = object(a, "metrics");
    metrics.insert("start".into(), json!(span.start as f64 / 1e9));
    metrics.insert("end".into(), json!(span.end as f64 / 1e9));
    let usage: [(&str, &[&str]); 3] = [
        (
            "prompt_tokens",
            &["gen_ai.usage.input_tokens", "gen_ai.usage.prompt_tokens"],
        ),
        (
            "completion_tokens",
            &[
                "gen_ai.usage.output_tokens",
                "gen_ai.usage.completion_tokens",
            ],
        ),
        ("tokens", &["gen_ai.usage.total_tokens"]),
    ];
    for (metric, keys) in usage {
        if let Some(n) = first(a, keys) {
            metrics.entry(metric).or_insert(n);
        }
    }
    let mut span_attributes = object(a, "span_attributes");
    span_attributes.entry("name").or_insert(json!(span.name));
    let kind = match a.get("gen_ai.operation.name").and_then(Value::as_str) {
        Some("chat" | "text_completion" | "generate_content") => Some("llm"),
        Some("execute_tool") => Some("tool"),
        _ => None,
    };
    if let Some(kind) = kind {
        span_attributes.entry("type").or_insert(json!(kind));
    }
    for (field, value) in [
        ("metadata", metadata),
        ("metrics", metrics),
        ("scores", object(a, "scores")),
        ("span_attributes", span_attributes),
    ] {
        if !value.is_empty() {
            row.insert(field.into(), Value::Object(value));
        }
    }

    if span.status.0 == 2 {
        let exception = span
            .events
            .iter()
            .find(|(name, _)| name == "exception")
            .and_then(|(_, attrs)| attrs.get("exception.message").cloned());
        let error = match (span.status.1.as_str(), exception) {
            (_, Some(message)) => message,
            ("", None) => json!("error"),
            (message, None) => json!(message),
        };
        row.insert("error".into(), error);
    }
    let parent = a
        .get("braintrust.parent")
        .and_then(Value::as_str)
        .map(str::to_owned);
    (parent, row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use proto::any_value::Value as V;

    fn kv(key: &str, value: V) -> proto::KeyValue {
        proto::KeyValue {
            key: key.into(),
            value: Some(proto::AnyValue { value: Some(value) }),
        }
    }

    #[test]
    fn protobuf_spans_decode_with_typed_attributes() {
        let span = proto::Span {
            trace_id: vec![0xab; 16],
            span_id: vec![0x01; 8],
            parent_span_id: vec![],
            name: "root".into(),
            start_time_unix_nano: 1_500_000_000,
            end_time_unix_nano: 2_000_000_000,
            attributes: vec![
                kv("s", V::StringValue("x".into())),
                kv("b", V::BoolValue(true)),
                kv("i", V::IntValue(-3)),
                kv("d", V::DoubleValue(0.5)),
                kv(
                    "a",
                    V::ArrayValue(proto::ArrayValue {
                        values: vec![
                            proto::AnyValue {
                                value: Some(V::IntValue(1)),
                            },
                            proto::AnyValue { value: None },
                        ],
                    }),
                ),
                kv(
                    "kv",
                    V::KvlistValue(proto::KeyValueList {
                        values: vec![kv("k", V::StringValue("v".into()))],
                    }),
                ),
                kv("bytes", V::BytesValue(vec![0xff])),
            ],
            events: vec![proto::Event {
                name: "exception".into(),
                attributes: vec![],
            }],
            status: Some(proto::Status {
                message: "m".into(),
                code: 2,
            }),
        };
        let request = proto::ExportTraceServiceRequest {
            resource_spans: vec![proto::ResourceSpans {
                scope_spans: vec![proto::ScopeSpans {
                    spans: vec![
                        span.clone(),
                        proto::Span {
                            status: None,
                            ..span
                        },
                    ],
                }],
            }],
        };
        let spans = decode_protobuf(&request.encode_to_vec()).unwrap();
        assert_eq!(spans.len(), 2);
        let s = &spans[0];
        assert_eq!(s.trace_id, "ab".repeat(16));
        assert_eq!(
            (s.span_id.as_str(), s.parent_span_id.as_str()),
            ("0101010101010101", "")
        );
        assert_eq!(
            (s.start, s.end, s.status.clone()),
            (1_500_000_000, 2_000_000_000, (2, "m".into()))
        );
        assert_eq!(
            Value::Object(s.attributes.clone()),
            json!({"s": "x", "b": true, "i": -3, "d": 0.5, "a": [1, null], "kv": {"k": "v"}, "bytes": "/w=="})
        );
        assert_eq!(s.events, vec![("exception".into(), Map::new())]);
        assert_eq!(spans[1].status, (0, String::new()));
        assert!(
            decode_protobuf(b"\xff\xff")
                .unwrap_err()
                .contains("invalid OTLP protobuf")
        );
    }

    #[test]
    fn json_spans_decode() {
        let body = json!({"resourceSpans": [{"scopeSpans": [{"spans": [{
            "traceId": "AB", "spanId": "01", "parentSpanId": "02", "name": "n",
            "startTimeUnixNano": "1000", "endTimeUnixNano": 2000,
            "attributes": [
                {"key": "s", "value": {"stringValue": "x"}},
                {"key": "i", "value": {"intValue": "7"}},
                {"key": "bad", "value": {"intValue": "x"}},
                {"key": "b", "value": {"boolValue": false}},
                {"key": "d", "value": {"doubleValue": 1.5}},
                {"key": "a", "value": {"arrayValue": {"values": [{"stringValue": "y"}]}}},
                {"key": "kv", "value": {"kvlistValue": {"values": [{"key": "k", "value": {"intValue": 1}}]}}},
                {"key": "none", "value": {}},
                {"value": {"stringValue": "keyless"}},
            ],
            "events": [{"name": "exception", "attributes": [{"key": "exception.message", "value": {"stringValue": "boom"}}]}],
            "status": {"code": 2},
        }]}]}]});
        let spans = decode_json(&body).unwrap();
        let s = &spans[0];
        assert_eq!(
            (
                s.trace_id.as_str(),
                s.span_id.as_str(),
                s.parent_span_id.as_str()
            ),
            ("ab", "01", "02")
        );
        assert_eq!(
            (s.start, s.end, s.status.clone()),
            (1000, 2000, (2, String::new()))
        );
        assert_eq!(
            Value::Object(s.attributes.clone()),
            json!({"s": "x", "i": 7, "bad": null, "b": false, "d": 1.5, "a": ["y"], "kv": {"k": 1}, "none": null})
        );
        assert_eq!(s.events[0].1["exception.message"], "boom");
        assert_eq!(decode_json(&json!({})).unwrap(), vec![]);
        assert!(decode_json(&json!([])).is_err());
    }

    fn span(attributes: Value) -> Span {
        Span {
            trace_id: "t".into(),
            span_id: "s".into(),
            name: "work".into(),
            start: 1_000_000_000,
            end: 3_500_000_000,
            attributes: attributes.as_object().unwrap().clone(),
            ..Span::default()
        }
    }

    #[test]
    fn braintrust_attributes_map_to_row_fields() {
        let (parent, row) = to_row(&span(json!({
            "braintrust.parent": "experiment_id:e",
            "braintrust.input_json": "{\"q\": 1}",
            "braintrust.output": "{\"score\": 1}",
            "braintrust.expected": "plain text",
            "braintrust.context_json": "{\"caller\": \"x\"}",
            "braintrust.tags": ["a", "b"],
            "braintrust.metadata": "{\"m\": 1}",
            "braintrust.metadata.extra": "e",
            "braintrust.scores": "{\"exact\": 0.5}",
            "braintrust.metrics": "{\"tokens\": 3}",
            "braintrust.span_attributes": "{\"type\": \"score\"}",
        })));
        assert_eq!(parent.as_deref(), Some("experiment_id:e"));
        assert_eq!(
            Value::Object(row),
            json!({
                "id": "s", "span_id": "s", "root_span_id": "t", "created": "1970-01-01T00:00:01.000Z",
                "input": {"q": 1}, "output": {"score": 1}, "expected": "plain text", "context": {"caller": "x"},
                "tags": ["a", "b"], "metadata": {"m": 1, "extra": "e"},
                "metrics": {"tokens": 3, "start": 1.0, "end": 3.5}, "scores": {"exact": 0.5},
                "span_attributes": {"type": "score", "name": "work"},
            })
        );
    }

    #[test]
    fn gen_ai_attributes_fill_in_llm_spans() {
        let mut s = span(json!({
            "gen_ai.operation.name": "chat",
            "gen_ai.request.model": "gpt",
            "gen_ai.input.messages": "[{\"role\": \"user\"}]",
            "gen_ai.completion": "hi",
            "gen_ai.usage.input_tokens": 2,
            "gen_ai.usage.completion_tokens": 3,
            "gen_ai.usage.total_tokens": 5,
        }));
        s.parent_span_id = "p".into();
        s.status = (2, "failed".into());
        let (parent, row) = to_row(&s);
        assert_eq!(parent, None);
        assert_eq!(row["span_parents"], json!(["p"]));
        assert_eq!(row["input"], json!([{"role": "user"}]));
        assert_eq!(row["output"], "hi");
        assert_eq!(row["metadata"], json!({"model": "gpt"}));
        assert_eq!(
            row["metrics"],
            json!({"start": 1.0, "end": 3.5, "prompt_tokens": 2, "completion_tokens": 3, "tokens": 5})
        );
        assert_eq!(
            row["span_attributes"],
            json!({"name": "work", "type": "llm"})
        );
        assert_eq!(row["error"], "failed");
        assert!(row.get("scores").is_none());

        let mut tool = span(json!({"gen_ai.operation.name": "execute_tool"}));
        tool.status = (2, String::new());
        let (_, row) = to_row(&tool);
        assert_eq!(
            (row["span_attributes"]["type"].clone(), row["error"].clone()),
            (json!("tool"), json!("error"))
        );
        tool.events = vec![(
            "exception".into(),
            json!({"exception.message": "boom"})
                .as_object()
                .unwrap()
                .clone(),
        )];
        assert_eq!(to_row(&tool).1["error"], "boom");
        let (_, row) = to_row(&span(json!({"gen_ai.operation.name": "other"})));
        assert_eq!(row["span_attributes"], json!({"name": "work"}));
    }
}

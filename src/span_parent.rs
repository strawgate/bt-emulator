//! Span parents as the SDKs pass them to invoke: the `{object_type, object_id,
//! row_ids, propagated_event}` struct, or the base64 string a span's `.export()`
//! returns (SpanComponents v3 or v4, py:span_identifier_v3.py / _v4.py).
//!
//! Binary layout: `[version, object_type, n, n × (field_id, bytes)…, json?]`, where
//! v3 fields are 16-byte UUIDs and v4 fields are 8-byte span ids or 16-byte trace
//! ids (field 3 / 4) or 16 bytes otherwise; anything not encoded in binary is in the
//! trailing JSON object.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

/// Where an invoked function's span goes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Parent {
    /// `experiment`, `project_logs` or `playground_logs`.
    pub object_type: String,
    pub object_id: Option<String>,
    /// A project named instead of identified (a logger exported before login).
    pub project_name: Option<String>,
    /// `(id, span_id, root_span_id)` of the row to nest under, if any.
    pub row: Option<(String, String, String)>,
    pub propagated_event: Option<Value>,
}

pub fn parse(value: &Value) -> Result<Parent, String> {
    match value {
        Value::String(s) => decode(s),
        Value::Object(_) => {
            // Index the Value, not the Map: a missing key is null, not a panic.
            let row = &value["row_ids"];
            Ok(Parent {
                object_type: value["object_type"].as_str().unwrap_or_default().to_owned(),
                object_id: value["object_id"].as_str().map(str::to_owned),
                project_name: None,
                row: row_ids(&row["id"], &row["span_id"], &row["root_span_id"]),
                propagated_event: Some(value["propagated_event"].clone()).filter(|v| !v.is_null()),
            })
        }
        other => Err(format!("invalid parent {other}")),
    }
}

fn row_ids(id: &Value, span: &Value, root: &Value) -> Option<(String, String, String)> {
    Some((
        id.as_str()?.to_owned(),
        span.as_str()?.to_owned(),
        root.as_str()?.to_owned(),
    ))
}

fn uuid(bytes: &[u8]) -> String {
    uuid::Uuid::from_slice(bytes).map_or_else(|_| hex::encode(bytes), |u| u.to_string())
}

fn decode(s: &str) -> Result<Parent, String> {
    let invalid = || format!("parent {s:?} is not an exported span (SpanComponents v3 or v4)");
    let raw = STANDARD.decode(s).map_err(|_| invalid())?;
    let (&version, rest) = raw.split_first().ok_or_else(invalid)?;
    if !(3..=4).contains(&version) || rest.len() < 2 {
        return Err(invalid());
    }
    let object_type = match rest[0] {
        1 => "experiment",
        2 => "project_logs",
        3 => "playground_logs",
        _ => return Err(invalid()),
    };
    let mut fields = Map::new();
    let mut at = 2;
    for _ in 0..rest[1] {
        let id = *rest.get(at).ok_or_else(invalid)?;
        let len = if version == 4 && id == 3 { 8 } else { 16 };
        let bytes = rest.get(at + 1..at + 1 + len).ok_or_else(invalid)?;
        let value = if version == 4 {
            hex::encode(bytes)
        } else {
            uuid(bytes)
        };
        let name = match id {
            1 => "object_id",
            2 => "row_id",
            3 => "span_id",
            4 => "root_span_id",
            _ => return Err(invalid()),
        };
        fields.insert(name.into(), json!(value));
        at += 1 + len;
    }
    if at < rest.len() {
        let Ok(Value::Object(extra)) = serde_json::from_slice(&rest[at..]) else {
            return Err(invalid());
        };
        fields.extend(extra);
    }
    let get = |k: &str| fields.get(k).unwrap_or(&Value::Null);
    let field = |k: &str| get(k).as_str().map(str::to_owned);
    Ok(Parent {
        object_type: object_type.to_owned(),
        object_id: field("object_id"),
        project_name: get("compute_object_metadata_args")["project_name"]
            .as_str()
            .map(str::to_owned),
        row: row_ids(get("row_id"), get("span_id"), get("root_span_id")),
        propagated_event: fields.get("propagated_event").cloned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Produced by the Python SDK's SpanComponentsV3/V4 `to_str()`.
    const V3: &str = "AwEEARERERERERERERERERERERECIiIiIiIiIiIiIiIiIiIiIgMzMzMzMzMzMzMzMzMzMzMzBEREREREREREREREREREREQ=";
    const V4: &str = "BAECAwEjRWeJq83vBAEjRWeJq83vASNFZ4mrze97InByb3BhZ2F0ZWRfZXZlbnQiOnsic3Bhbl9hdHRyaWJ1dGVzIjp7InB1cnBvc2UiOiJzY29yZXIifX0sIm9iamVjdF9pZCI6IjExMTExMTExLTExMTEtMTExMS0xMTExLTExMTExMTExMTExMSIsInJvd19pZCI6InIxIn0=";
    const V4_LAZY: &str =
        "BAIAeyJjb21wdXRlX29iamVjdF9tZXRhZGF0YV9hcmdzIjp7InByb2plY3RfbmFtZSI6ImxhenkifX0=";

    #[test]
    fn exported_spans_decode() {
        let p = parse(&json!(V3)).unwrap();
        assert_eq!(p.object_type, "experiment");
        assert_eq!(
            p.object_id.as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        let row = p.row.unwrap();
        assert_eq!(row.1, "33333333-3333-3333-3333-333333333333");
        assert_eq!(row.2, "44444444-4444-4444-4444-444444444444");

        let p = parse(&json!(V4)).unwrap();
        assert_eq!(
            p.row,
            Some((
                "r1".into(),
                "0123456789abcdef".into(),
                "0123456789abcdef0123456789abcdef".into()
            ))
        );
        assert_eq!(
            p.propagated_event,
            Some(json!({"span_attributes": {"purpose": "scorer"}}))
        );
        assert_eq!(
            p.object_id.as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );

        let p = parse(&json!(V4_LAZY)).unwrap();
        assert_eq!(
            (
                p.object_type.as_str(),
                p.project_name.as_deref(),
                p.object_id
            ),
            ("project_logs", Some("lazy"), None)
        );
        assert_eq!(p.row, None);

        let playground = STANDARD.encode([4, 3, 0]);
        assert_eq!(
            parse(&json!(playground)).unwrap().object_type,
            "playground_logs"
        );
    }

    #[test]
    fn parent_structs_parse() {
        let p = parse(&json!({"object_type": "project_logs", "object_id": "p",
            "row_ids": {"id": "r", "span_id": "s", "root_span_id": "t"}, "propagated_event": null}))
        .unwrap();
        assert_eq!(p.row, Some(("r".into(), "s".into(), "t".into())));
        assert_eq!(p.propagated_event, None);
        assert_eq!(
            parse(&json!({"object_type": "experiment", "object_id": "e"}))
                .unwrap()
                .row,
            None
        );
    }

    #[test]
    fn malformed_parents_are_errors() {
        let enc = |bytes: &[u8]| json!(STANDARD.encode(bytes));
        for bad in [
            json!(5),
            json!("not base64!"),
            enc(&[]),
            enc(&[2, 1, 0]),
            enc(&[4, 9, 0]),
            enc(&[4, 1]),
            enc(&[4, 1, 1, 7]),
            enc(&[4, 1, 1, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            enc(&[3, 1, 1, 1, 0]),
            enc(b"\x04\x01\x00[1]"),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }
        // A v3 field that is not a UUID-shaped 16 bytes still decodes.
        assert_eq!(uuid(&[0xab; 3]), "ababab");
    }
}

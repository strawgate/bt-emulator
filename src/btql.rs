//! The subset of structured BTQL the SDKs send: `select *` from one object, with
//! an optional filter expression built from and/or/not, comparisons, `in`, and
//! null checks over `ident` paths and `literal` values.

use std::cmp::Ordering;

use serde_json::Value;

#[derive(Debug, PartialEq)]
pub struct Query {
    pub object_type: String,
    pub object_id: String,
    pub filter: Option<Value>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

pub fn parse(body: &Value) -> Result<Query, String> {
    let query = &body["query"];
    if let Some(text) = query.as_str() {
        return crate::btql_text::parse(text);
    }
    let from = &query["from"];
    if from["op"] != "function" {
        return Err("query.from must be a function call like dataset('<id>')".into());
    }
    let object_type = from["name"]["name"][0]
        .as_str()
        .ok_or("query.from.name is missing")?
        .to_owned();
    let object_id = from["args"][0]["value"]
        .as_str()
        .ok_or("query.from.args[0] must be a literal id")?
        .to_owned();
    Ok(Query {
        object_type,
        object_id,
        filter: Some(query["filter"].clone()).filter(|f| !f.is_null()),
        limit: query["limit"].as_u64().map(|l| l as usize),
        cursor: query["cursor"].as_str().map(str::to_owned),
    })
}

/// Evaluate a filter expression against a row.
pub fn matches(expr: &Value, row: &Value) -> Result<bool, String> {
    let op = expr["op"]
        .as_str()
        .ok_or("filter expression is missing `op`")?;
    let children = || {
        expr["children"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
    };
    Ok(match op {
        "and" => {
            for c in children() {
                if !matches(c, row)? {
                    return Ok(false);
                }
            }
            true
        }
        "or" => {
            for c in children() {
                if matches(c, row)? {
                    return Ok(true);
                }
            }
            false
        }
        "not" => !matches(&expr["expr"], row)?,
        // A bare field or literal used as a condition.
        "ident" | "literal" => eval(expr, row)? == Value::Bool(true),
        "isnull" => eval(&expr["expr"], row)?.is_null(),
        "isnotnull" => !eval(&expr["expr"], row)?.is_null(),
        "eq" => eval(&expr["left"], row)? == eval(&expr["right"], row)?,
        "ne" => eval(&expr["left"], row)? != eval(&expr["right"], row)?,
        "in" => {
            let needle = eval(&expr["left"], row)?;
            match eval(&expr["right"], row)? {
                Value::Array(haystack) => haystack.contains(&needle),
                _ => return Err("`in` requires an array on the right".into()),
            }
        }
        "gt" | "ge" | "lt" | "le" => {
            let ord = compare(&eval(&expr["left"], row)?, &eval(&expr["right"], row)?);
            match (op, ord) {
                (_, None) => false,
                ("gt", Some(o)) => o == Ordering::Greater,
                ("ge", Some(o)) => o != Ordering::Less,
                ("lt", Some(o)) => o == Ordering::Less,
                (_, Some(o)) => o != Ordering::Greater,
            }
        }
        other => return Err(format!("unsupported BTQL operator `{other}`")),
    })
}

fn eval(expr: &Value, row: &Value) -> Result<Value, String> {
    match expr["op"].as_str() {
        Some("literal") => Ok(expr["value"].clone()),
        Some("ident") => {
            let path = expr["name"]
                .as_array()
                .ok_or("ident.name must be an array")?;
            let mut cur = row;
            for key in path {
                cur = match key {
                    Value::String(k) => &cur[k.as_str()],
                    Value::Number(n) => &cur[n.as_u64().unwrap_or(u64::MAX) as usize],
                    _ => &Value::Null,
                };
            }
            Ok(cur.clone())
        }
        _ => Ok(Value::Bool(matches(expr, row)?)),
    }
}

fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ident(path: &[&str]) -> Value {
        json!({"op": "ident", "name": path})
    }
    fn lit(v: Value) -> Value {
        json!({"op": "literal", "value": v})
    }
    fn check(expr: Value, row: &Value) -> bool {
        matches(&expr, row).unwrap()
    }

    #[test]
    fn parses_sdk_object_fetch_query() {
        let body = json!({"query": {
            "select": [{"op": "star"}],
            "from": {"op": "function", "name": {"op": "ident", "name": ["dataset"]}, "args": [{"op": "literal", "value": "d1"}]},
            "cursor": null, "limit": 5,
        }});
        assert_eq!(
            parse(&body).unwrap(),
            Query {
                object_type: "dataset".into(),
                object_id: "d1".into(),
                filter: None,
                limit: Some(5),
                cursor: None
            }
        );
        let mut with_filter = body.clone();
        with_filter["query"]["filter"] = json!({"op": "eq"});
        with_filter["query"]["cursor"] = json!("c");
        let q = parse(&with_filter).unwrap();
        assert_eq!(
            (q.filter, q.cursor.as_deref()),
            (Some(json!({"op": "eq"})), Some("c"))
        );
    }

    #[test]
    fn rejects_unsupported_queries() {
        assert!(
            parse(&json!({"query": "select: *"}))
                .unwrap_err()
                .contains("unexpected character")
        );
        assert_eq!(
            parse(&json!({"query": "SELECT * FROM dataset('d')"}))
                .unwrap()
                .object_id,
            "d"
        );
        assert!(
            parse(&json!({"query": {"from": {"op": "ident"}}}))
                .unwrap_err()
                .contains("function call")
        );
        assert!(
            parse(&json!({"query": {"from": {"op": "function"}}}))
                .unwrap_err()
                .contains("name")
        );
        let no_id = json!({"query": {"from": {"op": "function", "name": {"name": ["dataset"]}}}});
        assert!(parse(&no_id).unwrap_err().contains("literal id"));
    }

    #[test]
    fn evaluates_the_sdk_span_filter() {
        // Literal filter from braintrust-sdk-python test_span_filters_backend_parity.
        let filter = json!({"op": "and", "children": [
            {"op": "eq", "left": ident(&["root_span_id"]), "right": lit(json!("r"))},
            {"op": "or", "children": [
                {"op": "isnull", "expr": ident(&["span_attributes", "purpose"])},
                {"op": "ne", "left": ident(&["span_attributes", "purpose"]), "right": lit(json!("scorer"))},
            ]},
            {"op": "in", "left": ident(&["span_attributes", "type"]), "right": lit(json!(["tool"]))},
        ]});
        let tool = json!({"root_span_id": "r", "span_attributes": {"type": "tool"}});
        let scorer =
            json!({"root_span_id": "r", "span_attributes": {"type": "tool", "purpose": "scorer"}});
        let other_root = json!({"root_span_id": "x", "span_attributes": {"type": "tool"}});
        assert!(check(filter.clone(), &tool));
        assert!(!check(filter.clone(), &scorer));
        assert!(!check(filter, &other_root));
    }

    #[test]
    fn comparisons_not_and_nested_boolean_values() {
        let row = json!({"n": 5, "s": "b", "arr": [10, 20], "flag": true});
        for (op, rhs, want) in [
            ("gt", 4, true),
            ("ge", 5, true),
            ("lt", 5, false),
            ("le", 5, true),
            ("gt", 9, false),
        ] {
            assert_eq!(
                check(
                    json!({"op": op, "left": ident(&["n"]), "right": lit(json!(rhs))}),
                    &row
                ),
                want,
                "{op}"
            );
        }
        assert!(check(
            json!({"op": "lt", "left": ident(&["s"]), "right": lit(json!("c"))}),
            &row
        ));
        assert!(!check(
            json!({"op": "lt", "left": ident(&["s"]), "right": lit(json!(1))}),
            &row
        ));
        assert!(check(
            json!({"op": "isnotnull", "expr": ident(&["n"])}),
            &row
        ));
        assert!(check(
            json!({"op": "not", "expr": {"op": "isnull", "expr": ident(&["n"])}}),
            &row
        ));
        let idx = json!({"op": "eq", "left": {"op": "ident", "name": ["arr", 1]}, "right": lit(json!(20))});
        assert!(check(idx, &row));
        let bad_key = json!({"op": "isnull", "expr": {"op": "ident", "name": ["arr", null]}});
        assert!(check(bad_key, &row));
        let nested = json!({"op": "eq", "left": {"op": "isnull", "expr": ident(&["missing"])}, "right": ident(&["flag"])});
        assert!(check(nested, &row));
        assert!(check(json!({"op": "and"}), &row));
        assert!(!check(json!({"op": "or"}), &row));
    }

    #[test]
    fn malformed_filters_are_errors() {
        let row = json!({});
        assert!(
            matches(&json!({}), &row)
                .unwrap_err()
                .contains("missing `op`")
        );
        assert!(
            matches(&json!({"op": "regex"}), &row)
                .unwrap_err()
                .contains("unsupported")
        );
        let bad_in = json!({"op": "in", "left": lit(json!(1)), "right": lit(json!(1))});
        assert!(matches(&bad_in, &row).unwrap_err().contains("array"));
        let bad_ident = json!({"op": "isnull", "expr": {"op": "ident", "name": "x"}});
        assert!(
            matches(&bad_ident, &row)
                .unwrap_err()
                .contains("ident.name")
        );
    }
}

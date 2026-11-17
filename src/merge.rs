//! Row merge semantics, mirroring Braintrust's `merge_dicts` (py:util.py):
//! objects merge recursively, other values replace, top-level `tags` arrays are
//! set-unioned, and any path listed in `_merge_paths` is replaced wholesale.

use serde_json::{Map, Value};

pub fn merge_row(
    dst: &mut Map<String, Value>,
    src: Map<String, Value>,
    merge_paths: &[Vec<String>],
) {
    merge_at(dst, src, merge_paths, &mut Vec::new());
}

fn merge_at(
    dst: &mut Map<String, Value>,
    src: Map<String, Value>,
    merge_paths: &[Vec<String>],
    path: &mut Vec<String>,
) {
    for (key, value) in src {
        path.push(key.clone());
        let replace = merge_paths.iter().any(|p| p == path);
        let merged = match (dst.remove(&key), value) {
            (Some(Value::Object(mut d)), Value::Object(s)) if !replace => {
                merge_at(&mut d, s, merge_paths, path);
                Value::Object(d)
            }
            (Some(Value::Array(mut d)), Value::Array(s))
                if !replace && path.len() == 1 && key == "tags" =>
            {
                for tag in s {
                    if !d.contains(&tag) {
                        d.push(tag);
                    }
                }
                Value::Array(d)
            }
            (_, value) => value,
        };
        dst.insert(key, merged);
        path.pop();
    }
}

/// Parse `_merge_paths` (a list of key paths) from a row, ignoring malformed entries.
pub fn merge_paths(value: Option<&Value>) -> Vec<Vec<String>> {
    let Some(Value::Array(paths)) = value else {
        return Vec::new();
    };
    paths
        .iter()
        .filter_map(|p| p.as_array())
        .map(|p| {
            p.iter()
                .filter_map(|k| k.as_str().map(str::to_owned))
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn merged(dst: Value, src: Value, paths: Value) -> Value {
        let mut d = dst.as_object().unwrap().clone();
        merge_row(
            &mut d,
            src.as_object().unwrap().clone(),
            &merge_paths(Some(&paths)),
        );
        Value::Object(d)
    }

    #[test]
    fn objects_merge_recursively_and_scalars_replace() {
        let out = merged(
            json!({"metrics": {"start": 1, "x": {"a": 1}}, "output": "old"}),
            json!({"metrics": {"end": 2, "x": {"b": 2}}, "output": "new"}),
            json!([]),
        );
        assert_eq!(
            out,
            json!({"metrics": {"start": 1, "end": 2, "x": {"a": 1, "b": 2}}, "output": "new"})
        );
    }

    #[test]
    fn top_level_tags_union_but_nested_arrays_replace() {
        let out = merged(
            json!({"tags": ["a", "b"], "metadata": {"tags": [1]}}),
            json!({"tags": ["b", "c"], "metadata": {"tags": [2]}}),
            json!([]),
        );
        assert_eq!(
            out,
            json!({"tags": ["a", "b", "c"], "metadata": {"tags": [2]}})
        );
    }

    #[test]
    fn merge_paths_replace_wholesale() {
        let out = merged(
            json!({"metadata": {"a": 1}, "tags": ["a"]}),
            json!({"metadata": {"b": 2}, "tags": ["b"]}),
            json!([["metadata"], ["tags"], "junk", [1]]),
        );
        assert_eq!(out, json!({"metadata": {"b": 2}, "tags": ["b"]}));
    }

    #[test]
    fn non_array_merge_paths_are_ignored() {
        assert!(merge_paths(Some(&json!("x"))).is_empty());
        assert!(merge_paths(None).is_empty());
    }
}

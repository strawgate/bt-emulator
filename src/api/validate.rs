//! Request validation for `/v1/*` against Braintrust's published OpenAPI spec
//! (compat/openapi/spec.json, Apache-2.0, vendored at a pinned commit).
//!
//! Each operation's query parameters and JSON body are checked with the spec's own
//! JSON Schemas, so invalid input is rejected with 400 exactly where the spec says
//! it should be. Unknown parameters and extra body fields are allowed, as in the spec.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonschema::{Draft, Validator};
use serde_json::{Value, json};

use super::{ApiError, MAX_BODY_BYTES};

static SPEC: LazyLock<Value> = LazyLock::new(|| {
    let mut spec = serde_json::from_str(include_str!("../../compat/openapi/spec.json"))
        .expect("vendored OpenAPI spec is valid JSON");
    correct(&mut spec);
    spec
});

/// Where recorded traffic shows the real API accepts more than the spec says
/// (compat/openapi/README.md, "Spec corrections").
fn correct(spec: &mut Value) {
    // The Go SDK creates prompts with `function_type: "prompt"`.
    for name in ["FunctionTypeEnum", "FunctionTypeEnumNullish"] {
        let values = spec["components"]["schemas"][name]["enum"]
            .as_array_mut()
            .expect("function type enum");
        values.insert(0, json!("prompt"));
    }
}

/// A named schema from the spec's components.
pub fn schema(name: &str) -> &'static Value {
    &SPEC["components"]["schemas"][name]
}

/// The spec's schema for one row of `object_type` (`experiment`, `dataset` or
/// `project_logs`), as BTQL returns it alongside the rows.
pub fn event_schema(object_type: &str) -> Value {
    let name = match object_type {
        "experiment" => "ExperimentEvent",
        "dataset" => "DatasetEvent",
        _ => "ProjectLogsEvent",
    };
    let properties = schema(name)["properties"].clone();
    json!({"type": "array", "items": {"type": "object", "properties": properties}})
}

struct Param {
    name: String,
    required: bool,
    validator: Validator,
}

struct Operation {
    params: Vec<Param>,
    /// `(required, validator)` for the JSON request body.
    body: Option<(bool, Validator)>,
}

type Cache = Mutex<HashMap<(String, String), Option<Arc<Operation>>>>;
static OPERATIONS: LazyLock<Cache> = LazyLock::new(Cache::default);

fn escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

/// A validator for the schema at a JSON pointer inside the spec. The spec document
/// itself is the root resource, so its internal `$ref`s resolve.
fn validator(pointer: &str) -> Validator {
    let mut doc = SPEC.clone();
    // A pointer inside a URI fragment must percent-encode `{` and `}`.
    let fragment = pointer.replace('{', "%7B").replace('}', "%7D");
    doc["$ref"] = json!(format!("#{fragment}"));
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .should_validate_formats(true)
        .build(&doc)
        .expect("spec schemas compile")
}

/// Resolve a local `$ref` (`#/a/b`) to its pointer, or keep the given pointer.
fn deref(value: &Value, pointer: String) -> (String, &'static Value) {
    match value["$ref"].as_str().and_then(|r| r.strip_prefix('#')) {
        Some(target) => (
            target.to_owned(),
            SPEC.pointer(target).unwrap_or(&Value::Null),
        ),
        None => {
            let target = SPEC.pointer(&pointer).unwrap_or(&Value::Null);
            (pointer, target)
        }
    }
}

/// The spec path template matching a concrete path, preferring literal segments.
fn template(path: &str) -> Option<&'static str> {
    let segments: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    SPEC["paths"]
        .as_object()?
        .keys()
        .filter_map(|t| {
            let parts: Vec<&str> = t.split('/').collect();
            let fits = parts.len() == segments.len()
                && parts
                    .iter()
                    .zip(&segments)
                    .all(|(p, s)| p.starts_with('{') || p == s);
            fits.then(|| {
                (
                    parts.iter().filter(|p| !p.starts_with('{')).count(),
                    t.as_str(),
                )
            })
        })
        .max_by_key(|(literals, _)| *literals)
        .map(|(_, t)| t)
}

fn build(template: &str, method: &str) -> Option<Operation> {
    let base = format!("/paths/{}", escape(template));
    let op = SPEC.pointer(&format!("{base}/{method}"))?;
    let mut params = Vec::new();
    for (scope, list) in [
        (base.clone(), &SPEC.pointer(&base)?["parameters"]),
        (format!("{base}/{method}"), &op["parameters"]),
    ] {
        for (i, p) in list.as_array().into_iter().flatten().enumerate() {
            let (pointer, param) = deref(p, format!("{scope}/parameters/{i}"));
            if param["in"] == "query" {
                params.push(Param {
                    name: param["name"].as_str().unwrap_or_default().to_owned(),
                    required: param["required"] == true,
                    validator: validator(&format!("{pointer}/schema")),
                });
            }
        }
    }
    let body = op.get("requestBody").map(|b| {
        let (pointer, body) = deref(b, format!("{base}/{method}/requestBody"));
        (
            body["required"] == true,
            validator(&format!("{pointer}/content/application~1json/schema")),
        )
    });
    Some(Operation { params, body })
}

fn operation(method: &str, path: &str) -> Option<Arc<Operation>> {
    let template = template(path)?;
    let key = (method.to_owned(), template.to_owned());
    let mut cache = OPERATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache
        .entry(key)
        .or_insert_with(|| build(template, method).map(Arc::new))
        .clone()
}

/// The ways a raw query value can be read: as a string, as a JSON scalar, and (for
/// repeated keys or single values) as an array.
fn readings(raw: &[&str]) -> Vec<Value> {
    let scalar = |s: &str| {
        serde_json::from_str::<Value>(s)
            .ok()
            .filter(|v| !v.is_object() && !v.is_array())
    };
    let strings = Value::Array(raw.iter().map(|s| json!(s)).collect());
    let typed = Value::Array(
        raw.iter()
            .map(|s| scalar(s).unwrap_or_else(|| json!(s)))
            .collect(),
    );
    let mut out = Vec::new();
    if let [one] = raw {
        out.push(json!(one));
        out.extend(scalar(one));
    }
    out.extend([strings, typed]);
    out
}

fn invalid(location: &str, error: impl std::fmt::Display) -> Response {
    ApiError::bad_request(format!("{location}: {error}")).into_response()
}

pub async fn middleware(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let Some(op) = path
        .starts_with("/v1/")
        .then(|| operation(&req.method().as_str().to_lowercase(), &path))
        .flatten()
    else {
        return next.run(req).await;
    };
    let query: Vec<(String, String)> =
        form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes())
            .into_owned()
            .collect();
    for param in &op.params {
        let raw: Vec<&str> = query
            .iter()
            .filter(|(k, _)| *k == param.name)
            .map(|(_, v)| v.as_str())
            .collect();
        if raw.is_empty() {
            if param.required {
                return invalid(&param.name, "is required");
            }
            continue;
        }
        let readings = readings(&raw);
        if !readings.iter().any(|v| param.validator.is_valid(v)) {
            let error = param
                .validator
                .validate(&readings[0])
                .err()
                .map_or_else(String::new, |e| e.to_string());
            return invalid(&format!("query parameter `{}`", param.name), error);
        }
    }
    let Some((required, validator)) = &op.body else {
        return next.run(req).await;
    };
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(err) => return invalid("body", err),
    };
    if bytes.trim_ascii().is_empty() {
        if *required {
            return invalid("body", "is required");
        }
    } else {
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(err) => return invalid("body", format!("invalid JSON: {err}")),
        };
        if let Err(err) = validator.validate(&value) {
            return invalid(&format!("body{}", err.instance_path()), err);
        }
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// Every route except the banner requires a bearer token, as the real API does. Any
/// token is accepted unless the emulator was started with an API key.
pub async fn require_auth(
    State(api_key): State<Option<Arc<str>>>,
    req: Request,
    next: Next,
) -> Response {
    // Signed attachment URLs carry their own token instead of an API key.
    if req.uri().path() == "/" || req.uri().path().starts_with("/attachment/blob/") {
        return next.run(req).await;
    }
    let token = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok());
    let token = token.map(|t| t.strip_prefix("Bearer ").unwrap_or(t));
    let message = match (token, api_key.as_deref()) {
        (None, _) => "missing Authorization header",
        (Some(given), Some(key)) if given != key => "invalid API key",
        _ => return next.run(req).await,
    };
    ApiError::new(axum::http::StatusCode::UNAUTHORIZED, message).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_prefer_literal_segments() {
        assert_eq!(
            template("/v1/project/abc"),
            Some("/v1/project/{project_id}")
        );
        assert_eq!(
            template("/v1/project_logs/abc/fetch"),
            Some("/v1/project_logs/{project_id}/fetch")
        );
        assert_eq!(template("/v1/experiment/"), Some("/v1/experiment"));
        assert_eq!(template("/v1/nope/a/b/c/d"), None);
        assert_eq!(escape("/v1/a~b"), "~1v1~1a~0b");
    }

    #[test]
    fn query_values_are_read_every_plausible_way() {
        assert_eq!(
            readings(&["5"]),
            vec![json!("5"), json!(5), json!(["5"]), json!([5])]
        );
        assert_eq!(
            readings(&["a", "true"]),
            vec![json!(["a", "true"]), json!(["a", true])]
        );
        assert_eq!(
            readings(&["{}"]),
            vec![json!("{}"), json!(["{}"]), json!(["{}"])]
        );
    }

    #[test]
    fn unknown_methods_have_no_operation() {
        assert!(operation("put", "/v1/project/abc").is_none());
        assert!(build("/v1/nope", "get").is_none());
        let (pointer, _) = deref(&json!({}), "/paths".into());
        assert_eq!(pointer, "/paths");
    }
}

//! `/v1/function` and `/v1/prompt`. Prompts are functions whose `function_data.type`
//! is `prompt`; both live in one versioned log per project, keyed by function id.
//! Every write appends a version, and a function's id is stable per (project, slug).
//! See docs/protocol.md §15.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::{StatusCode, Uri};
use serde_json::{Map, Value, json};

use super::extract::{JsonBody, Params};
use super::objects::{check_uuid, new_id, paginate, require_parent};
use super::rows::parse_version;
use super::{ApiError, ApiResult, ORG_ID};
use crate::db::Db;
use crate::db::events::Event;
use crate::db::objects::Kind;
use crate::error::Error;

pub(super) const OBJECT_TYPE: &str = "functions";

/// Optional fields reported as `null` when unset, per endpoint.
const PROMPT_FIELDS: [&str; 6] = [
    "description",
    "function_data",
    "function_type",
    "metadata",
    "prompt_data",
    "tags",
];
const FUNCTION_FIELDS: [&str; 8] = [
    "description",
    "function_data",
    "function_schema",
    "function_type",
    "metadata",
    "origin",
    "prompt_data",
    "tags",
];

/// Fields a write may not set directly.
const IMMUTABLE: [&str; 7] = [
    "id",
    "org_id",
    "project_id",
    "log_id",
    "created",
    "_xact_id",
    "_object_delete",
];

/// Which endpoint was called: `/v1/prompt` lists only prompts, though any function
/// can be read, patched or deleted through either.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Flavor {
    Function,
    Prompt,
}

impl Flavor {
    fn of(uri: &Uri) -> Self {
        if uri.path().starts_with("/v1/prompt") {
            Self::Prompt
        } else {
            Self::Function
        }
    }

    fn lists(self, data: &Map<String, Value>) -> bool {
        self == Self::Function || data["function_data"]["type"] == "prompt"
    }

    /// The full API representation of a stored version.
    fn shape(self, event: Event) -> Value {
        let mut data = event.data;
        let fields: &[&str] = match self {
            Self::Function => &FUNCTION_FIELDS,
            Self::Prompt => &PROMPT_FIELDS,
        };
        for key in fields {
            data.entry(*key).or_insert(Value::Null);
        }
        with_xact(data, event.xact_id)
    }

    fn missing(self) -> ApiError {
        let what = match self {
            Self::Function => "Function",
            Self::Prompt => "Prompt",
        };
        ApiError {
            status: StatusCode::BAD_REQUEST,
            body: json!({"Code": "BadRequestError", "Message": format!("{what} does not exist or you do not have access")}),
        }
    }
}

fn with_xact(mut data: Map<String, Value>, xact_id: i64) -> Value {
    data.insert("_xact_id".into(), json!(xact_id.to_string()));
    Value::Object(data)
}

/// Append a version (`None` deletes). Callers hold the write lock.
async fn put(
    db: &Db,
    project_id: &str,
    id: &str,
    data: Option<&Map<String, Value>>,
) -> ApiResult<i64> {
    let mut tx = db.begin().await?;
    let xact = Db::next_xact(&mut tx).await?;
    Db::put_row(&mut tx, OBJECT_TYPE, project_id, id, xact, 0, data).await?;
    tx.commit().await.map_err(Error::from)?;
    Ok(xact)
}

/// A function's current version, or its version as of `version`.
pub(super) async fn current(
    db: &Db,
    flavor: Flavor,
    id: &str,
    version: Option<i64>,
) -> ApiResult<Event> {
    check_uuid(id)?;
    match db.row_at(OBJECT_TYPE, id, version).await? {
        Some((_, event)) => Ok(event),
        None => Err(flavor.missing()),
    }
}

/// `POST` and `PUT`: both create the function, or append a new version of the one
/// with the same slug in the project.
pub async fn create(
    State(db): State<Db>,
    uri: Uri,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let flavor = Flavor::of(&uri);
    let project = require_parent(&db, body["project_id"].as_str().unwrap_or_default()).await?;
    let slug = body["slug"].as_str().unwrap_or_default();
    let _guard = db.write_lock().await;
    let existing = db
        .all_rows(OBJECT_TYPE, project.id())
        .await?
        .into_iter()
        .find(|e| e.data["slug"] == slug);
    let id = existing.map_or_else(new_id, |e| {
        e.data["id"].as_str().unwrap_or_default().to_owned()
    });
    let mut data = Map::new();
    for (key, value) in body.as_object().cloned().unwrap_or_default() {
        if !value.is_null() && !IMMUTABLE.contains(&key.as_str()) {
            data.insert(key, value);
        }
    }
    if flavor == Flavor::Prompt {
        data.entry("function_data")
            .or_insert(json!({"type": "prompt"}));
    }
    data.extend([
        ("id".into(), json!(id)),
        ("org_id".into(), json!(ORG_ID)),
        ("project_id".into(), json!(project.id())),
        ("log_id".into(), json!("p")),
        ("created".into(), json!(crate::time::now_iso())),
    ]);
    let xact = put(&db, project.id(), &id, Some(&data)).await?;
    Ok(Json(with_xact(data, xact)))
}

pub async fn list(
    State(db): State<Db>,
    uri: Uri,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<Value>> {
    let flavor = Flavor::of(&uri);
    let params = Params::parse(query.as_deref());
    let version = parse_version(&params.get("version").map_or(Value::Null, |v| json!(v)))?;
    let project_id = match (params.get("project_id"), params.get("project_name")) {
        (Some(id), _) => id.to_owned(),
        (None, Some(name)) => match db.object_by_name(Kind::Project, ORG_ID, name).await? {
            Some(project) => project.id().to_owned(),
            None => return Ok(Json(json!({"objects": []}))),
        },
        (None, None) => String::new(),
    };
    let ids: Vec<&str> = params.all("ids").collect();
    let name = params.get("function_name").or(params.get("prompt_name"));
    let slug = params.get("slug");
    let objects: Vec<Value> = db
        .rows_page(OBJECT_TYPE, &project_id, version, None, usize::MAX)
        .await?
        .into_iter()
        .filter(|e| flavor.lists(&e.data))
        .filter(|e| ids.is_empty() || ids.iter().any(|id| e.data["id"] == *id))
        .filter(|e| name.is_none_or(|n| e.data["name"] == n))
        .filter(|e| slug.is_none_or(|s| e.data["slug"] == s))
        .map(|e| flavor.shape(e))
        .collect();
    Ok(Json(json!({"objects": paginate(objects, &params)})))
}

pub async fn get(
    State(db): State<Db>,
    uri: Uri,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<Value>> {
    let flavor = Flavor::of(&uri);
    let params = Params::parse(query.as_deref());
    let version = parse_version(&params.get("version").map_or(Value::Null, |v| json!(v)))?;
    Ok(Json(
        flavor.shape(current(&db, flavor, &id, version).await?),
    ))
}

/// Object fields are deep-merged; other fields replace. `null` is ignored. An object
/// with a different `type` (a tagged union variant, at any depth) replaces outright.
pub async fn patch(
    State(db): State<Db>,
    uri: Uri,
    Path(id): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let flavor = Flavor::of(&uri);
    let _guard = db.write_lock().await;
    let mut data = current(&db, flavor, &id, None).await?.data;
    let fields: Map<String, Value> = body
        .as_object()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|(k, v)| !v.is_null() && !IMMUTABLE.contains(&k.as_str()))
        .collect();
    let mut replace = Vec::new();
    retyped(&data, &fields, &mut Vec::new(), &mut replace);
    crate::merge::merge_row(&mut data, fields, &replace);
    let project_id = data["project_id"].as_str().unwrap_or_default().to_owned();
    let xact = put(&db, &project_id, &id, Some(&data)).await?;
    Ok(Json(with_xact(data, xact)))
}

/// Paths where a patch swaps one variant of a tagged union (an object's `type`) for
/// another: those are replaced, since merging two variants yields neither.
fn retyped(
    dst: &Map<String, Value>,
    src: &Map<String, Value>,
    path: &mut Vec<String>,
    out: &mut Vec<Vec<String>>,
) {
    for (key, value) in src {
        let (Some(Value::Object(d)), Value::Object(s)) = (dst.get(key), value) else {
            continue;
        };
        path.push(key.clone());
        if s.contains_key("type") && d.get("type") != s.get("type") {
            out.push(path.clone());
        } else {
            retyped(d, s, path, out);
        }
        path.pop();
    }
}

pub async fn delete(
    State(db): State<Db>,
    uri: Uri,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let flavor = Flavor::of(&uri);
    let _guard = db.write_lock().await;
    let data = current(&db, flavor, &id, None).await?.data;
    let project_id = data["project_id"].as_str().unwrap_or_default();
    let xact = put(&db, project_id, &id, None).await?;
    let mut out = data;
    out.insert("_object_delete".into(), json!(true));
    out.insert("created".into(), json!(crate::time::now_iso()));
    Ok(Json(with_xact(out, xact)))
}

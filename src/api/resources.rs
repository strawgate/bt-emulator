//! The named `/v1` resources beyond projects, experiments, datasets and functions:
//! automations, project scores and tags, views, roles, groups and the like. They
//! share Braintrust's conventions, so one implementation driven by the OpenAPI spec
//! serves them all (docs/protocol.md §18):
//! - `POST` creates, or returns the existing resource with that name in its scope.
//! - `PUT` creates, or replaces the existing one (keeping its id and `created`).
//! - `GET` lists (newest first) or reads one; `PATCH` replaces the given fields;
//!   `DELETE` removes it and returns it.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::{Method, StatusCode, Uri};
use serde_json::{Map, Value, json};

use super::extract::{JsonBody, Params};
use super::objects::{check_uuid, new_id, paginate, require_parent};
use super::{ApiError, ApiResult, ORG_ID, USER_ID};
use crate::db::Db;
use crate::db::objects::{Kind, Object, identity};

/// Each resource kind and the spec schema of its API representation.
pub const KINDS: [(&str, &str); 13] = [
    ("ai_secret", "AISecret"),
    ("dataset_snapshot", "DatasetSnapshot"),
    ("env_var", "EnvVar"),
    ("environment", "Environment"),
    ("group", "Group"),
    ("mcp_server", "MCPServer"),
    ("project_automation", "ProjectAutomation"),
    ("project_group", "ProjectGroup"),
    ("project_score", "ProjectScore"),
    ("project_tag", "ProjectTag"),
    ("role", "Role"),
    ("span_iframe", "SpanIFrame"),
    ("view", "View"),
];

/// Fields the server sets; requests cannot change them.
const SERVER_FIELDS: [&str; 5] = ["id", "created", "user_id", "org_id", "updated_at"];

/// The resource kind a request addresses (`/v1/<kind>[/<id>]`).
fn kind_of(uri: &Uri) -> &'static str {
    // `/v1/<kind>[/<id>]`, or `/environment[/<id>]`.
    let mut segments = uri.path().split('/').skip(1);
    let segment = match segments.next() {
        Some("v1") => segments.next().unwrap_or_default(),
        other => other.unwrap_or_default(),
    };
    KINDS
        .iter()
        .find(|(kind, _)| *kind == segment)
        .map_or("", |(kind, _)| kind)
}

/// The route prefix of a kind.
pub fn path(kind: &str) -> String {
    if kind == "environment" {
        "/environment".into()
    } else {
        format!("/v1/{kind}")
    }
}

fn schema(kind: &str) -> &'static Value {
    let name = KINDS
        .iter()
        .find(|(k, _)| *k == kind)
        .map_or("", |(_, s)| s);
    super::validate::schema(name)
}

fn properties(kind: &str) -> &'static Map<String, Value> {
    schema(kind)["properties"]
        .as_object()
        .expect("resource schemas have properties")
}

fn nullable(schema: &Value) -> bool {
    schema["type"]
        .as_array()
        .is_some_and(|types| types.iter().any(|t| t == "null"))
}

/// The scope names are unique within: the project, dataset or object the resource
/// belongs to, else the organization.
fn scope_key(data: &Value) -> String {
    match (
        data["project_id"].as_str(),
        data["dataset_id"].as_str(),
        data["object_type"].as_str(),
        data["object_id"].as_str(),
    ) {
        (Some(pid), ..) => pid.to_owned(),
        (None, Some(did), ..) => did.to_owned(),
        (None, None, Some(ty), Some(id)) => format!("{ty}:{id}"),
        _ => ORG_ID.to_owned(),
    }
}

/// [`scope_key`], after checking that a parent project or dataset exists.
async fn scope(db: &Db, body: &Value) -> ApiResult<String> {
    if let Some(pid) = body["project_id"].as_str() {
        require_parent(db, pid).await?;
    } else if let Some(id) = body["dataset_id"].as_str() {
        check_uuid(id)?;
        db.object(Kind::Dataset, id)
            .await?
            .ok_or_else(|| ApiError::missing_parent(Kind::Dataset, id))?;
    }
    Ok(scope_key(body))
}

/// The stored representation of `body`: the fields the resource's schema has, plus
/// the server's, with `null` for every unset nullable field.
pub(super) fn build(kind: &str, body: &Value, keep: Option<&Value>) -> Value {
    let props = properties(kind);
    let required = schema(kind)["required"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut data = Map::new();
    for (key, schema) in props {
        let value = match (keep.map(|k| &k[key]), &body[key]) {
            (Some(kept), _) if SERVER_FIELDS.contains(&key.as_str()) && key != "updated_at" => {
                kept.clone()
            }
            (_, v) if !v.is_null() && !SERVER_FIELDS.contains(&key.as_str()) => v.clone(),
            _ => match key.as_str() {
                "id" => json!(new_id()),
                "created" | "updated_at" => json!(crate::time::now_iso()),
                "user_id" => json!(USER_ID),
                "org_id" => json!(ORG_ID),
                _ if nullable(schema) => Value::Null,
                // A required list the request may omit starts out empty.
                _ if required.contains(&json!(key)) && schema["type"] == "array" => json!([]),
                _ => continue,
            },
        };
        data.insert(key.clone(), value);
    }
    Value::Object(data)
}

pub async fn create(
    State(db): State<Db>,
    method: Method,
    uri: Uri,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let kind = kind_of(&uri);
    let scope = scope(&db, &body).await?;
    let name = body[identity(kind)].as_str().unwrap_or_default();
    let _guard = db.write_lock().await;
    let data = match db.object_by_name_of(kind, &scope, name).await? {
        Some(existing) if method == Method::POST => existing.data,
        Some(existing) => {
            let data = build(kind, &body, Some(&existing.data));
            db.update_object(&data, false).await?;
            data
        }
        None => {
            let data = build(kind, &body, None);
            db.insert_object_of(kind, &scope, data).await?.data
        }
    };
    Ok(Json(data))
}

pub(super) async fn find(db: &Db, kind: &str, id: &str) -> ApiResult<Object> {
    check_uuid(id)?;
    db.object_of(kind, id)
        .await?
        .ok_or_else(|| ApiError::missing_named(kind, id))
}

pub async fn get(State(db): State<Db>, uri: Uri, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(find(&db, kind_of(&uri), &id).await?.data))
}

pub async fn list(
    State(db): State<Db>,
    uri: Uri,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<Value>> {
    let kind = kind_of(&uri);
    let params = Params::parse(query.as_deref());
    let ids: Vec<&str> = params.all("ids").collect();
    let name_param = if kind == "environment" {
        "name".into()
    } else {
        format!("{kind}_name")
    };
    let name = params.get(&name_param);
    let project_id = match (params.get("project_id"), params.get("project_name")) {
        (Some(id), _) => Some(id.to_owned()),
        (None, Some(name)) => match db.object_by_name(Kind::Project, ORG_ID, name).await? {
            Some(project) => Some(project.id().to_owned()),
            None => return Ok(Json(json!({"objects": []}))),
        },
        (None, None) => None,
    };
    // Exact-match filters on the resource's own fields.
    let filters: Vec<(&str, &str)> = ["object_type", "object_id", "view_type", "score_type"]
        .into_iter()
        .filter_map(|key| params.get(key).map(|v| (key, v)))
        .collect();
    let objects: Vec<Value> = db
        .objects_of(kind)
        .await?
        .into_iter()
        .map(|o| o.data)
        .filter(|o| ids.is_empty() || ids.iter().any(|id| o["id"] == *id))
        .filter(|o| name.is_none_or(|n| o["name"] == n))
        .filter(|o| project_id.as_deref().is_none_or(|p| o["project_id"] == p))
        .filter(|o| filters.iter().all(|(k, v)| o[*k] == *v))
        .collect();
    Ok(Json(json!({"objects": paginate(objects, &params)})))
}

pub async fn patch(
    State(db): State<Db>,
    uri: Uri,
    Path(id): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let kind = kind_of(&uri);
    let _guard = db.write_lock().await;
    let mut data = find(&db, kind, &id).await?.data;
    let props = properties(kind);
    let fields = body.as_object().cloned().unwrap_or_default();
    if let Some(name) = fields.get(identity(kind)).and_then(Value::as_str) {
        let taken = db.object_by_name_of(kind, &scope_key(&data), name).await?;
        if taken.is_some_and(|o| o.id() != id) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                format!("a {kind} named {name:?} already exists"),
            ));
        }
    }
    for (key, value) in fields {
        let scoping = ["project_id", "dataset_id", "object_type", "object_id"];
        // `add_<field>` / `remove_<field>` edit a list field's members.
        let edit = key
            .strip_prefix("add_")
            .map(|f| (f, true))
            .or_else(|| key.strip_prefix("remove_").map(|f| (f, false)));
        if let (Some((field, add)), Value::Array(items)) = (edit, &value) {
            let mut list = data[field].as_array().cloned().unwrap_or_default();
            for item in items {
                list.retain(|v| v != item);
                if add {
                    list.push(item.clone());
                }
            }
            data[field] = Value::Array(list);
            continue;
        }
        if props.contains_key(&key)
            && !SERVER_FIELDS.contains(&key.as_str())
            && !scoping.contains(&key.as_str())
            && (!value.is_null() || nullable(&props[&key]))
        {
            data[key] = value;
        }
    }
    if props.contains_key("updated_at") {
        data["updated_at"] = json!(crate::time::now_iso());
    }
    db.update_object(&data, false).await?;
    Ok(Json(data))
}

pub async fn delete(
    State(db): State<Db>,
    uri: Uri,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let kind = kind_of(&uri);
    remove(&db, find(&db, kind, &id).await?.data).await
}

/// `DELETE /v1/ai_secret` names the secret in the body instead of the path.
pub async fn delete_by_name(
    State(db): State<Db>,
    uri: Uri,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let kind = kind_of(&uri);
    let name = body["name"].as_str().unwrap_or_default();
    let object = db
        .object_by_name_of(kind, ORG_ID, name)
        .await?
        .ok_or_else(|| ApiError::missing_named(kind, name))?;
    remove(&db, object.data).await
}

pub(super) async fn remove(db: &Db, mut data: Value) -> ApiResult<Json<Value>> {
    if data.get("deleted_at").is_some() {
        data["deleted_at"] = json!(crate::time::now_iso());
    }
    db.update_object(&data, true).await?;
    Ok(Json(data))
}

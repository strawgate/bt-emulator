//! `/v1/{project,experiment,dataset}` CRUD and the object helpers the legacy
//! `/api/*` endpoints share. Semantics follow docs/protocol.md §3-5.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde_json::{Value, json};

use super::extract::{JsonBody, Params};
use super::{ApiError, ApiResult, ORG_ID, USER_ID};
use crate::db::Db;
use crate::db::objects::{Kind, Object};

pub fn parse_kind(kind: &str) -> ApiResult<Kind> {
    match kind {
        "project" => Ok(Kind::Project),
        "experiment" => Ok(Kind::Experiment),
        "dataset" => Ok(Kind::Dataset),
        other => Err(ApiError::not_found(format!(
            "bt-emulator does not implement /v1/{other}"
        ))),
    }
}

pub fn check_uuid(id: &str) -> ApiResult<()> {
    uuid::Uuid::parse_str(id).map(drop).map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        body: json!([{"code": "invalid_string", "validation": "uuid", "message": "Invalid uuid", "path": ["id"]}]),
    })
}

/// A live object by id, or Braintrust's 403.
pub async fn require(db: &Db, kind: Kind, id: &str) -> ApiResult<Object> {
    check_uuid(id)?;
    db.object(kind, id)
        .await?
        .ok_or_else(|| ApiError::missing(kind, id))
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn str_field<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
    body[key].as_str()
}

/// A field the spec marks required; requests were already validated (see `validate`).
fn required<'a>(body: &'a Value, key: &str) -> &'a str {
    str_field(body, key).unwrap_or_default()
}

pub async fn get_or_create_project(db: &Db, name: &str, body: &Value) -> ApiResult<(Object, bool)> {
    let _guard = db.write_lock().await;
    if let Some(existing) = db.object_by_name(Kind::Project, ORG_ID, name).await? {
        return Ok((existing, true));
    }
    let data = json!({
        "id": new_id(), "org_id": ORG_ID, "name": name, "description": body["description"],
        "created": crate::time::now_iso(), "deleted_at": null, "user_id": USER_ID, "settings": body["settings"],
    });
    Ok((db.insert_object(Kind::Project, ORG_ID, data).await?, false))
}

pub async fn get_or_create_dataset(
    db: &Db,
    project: &Object,
    name: &str,
    body: &Value,
) -> ApiResult<(Object, bool)> {
    let _guard = db.write_lock().await;
    if let Some(existing) = db.object_by_name(Kind::Dataset, project.id(), name).await? {
        return Ok((existing, true));
    }
    let data = json!({
        "id": new_id(), "project_id": project.id(), "name": name, "description": body["description"],
        "created": crate::time::now_iso(), "deleted_at": null, "user_id": USER_ID,
        "tags": body["tags"], "metadata": body["metadata"], "url_slug": slug(name),
    });
    Ok((
        db.insert_object(Kind::Dataset, project.id(), data).await?,
        false,
    ))
}

fn slug(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

const EXPERIMENT_FIELDS: [&str; 10] = [
    "description",
    "repo_info",
    "base_exp_id",
    "dataset_id",
    "dataset_version",
    "internal_metadata",
    "parameters_id",
    "parameters_version",
    "metadata",
    "tags",
];

/// Create an experiment. An existing name is reused unless `ensure_new`, in which
/// case the new experiment is named `<name>-<8 hex>`.
pub async fn create_experiment(
    db: &Db,
    project: &Object,
    name: Option<&str>,
    ensure_new: bool,
    body: &Value,
) -> ApiResult<Object> {
    let _guard = db.write_lock().await;
    let pid = project.id();
    let mut name = match name {
        Some(name) => name.to_owned(),
        None => {
            let branch = body["repo_info"]["branch"].as_str().unwrap_or("experiment");
            format!("{branch}-{}", crate::time::unix_secs())
        }
    };
    while let Some(existing) = db.object_by_name(Kind::Experiment, pid, &name).await? {
        if !ensure_new {
            return Ok(existing);
        }
        name = format!("{name}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    }
    let mut data = json!({
        "id": new_id(), "project_id": pid, "name": name, "created": crate::time::now_iso(),
        "commit": body["repo_info"]["commit"], "deleted_at": null,
        "public": body["public"].as_bool().unwrap_or(false), "user_id": USER_ID,
    });
    for key in EXPERIMENT_FIELDS {
        data[key] = body[key].clone();
    }
    if data["repo_info"].is_null() {
        data["repo_info"] = json!({});
    }
    if data["base_exp_id"].is_null() {
        // Default comparison target: the project's most recent experiment.
        let latest = db
            .objects(Kind::Experiment)
            .await?
            .into_iter()
            .find(|o| o.data["project_id"] == pid);
        data["base_exp_id"] = latest.map_or(Value::Null, |o| json!(o.id()));
    }
    Ok(db.insert_object(Kind::Experiment, pid, data).await?)
}

/// Resolve a project from an id or a name, optionally creating it by name.
pub async fn project_ref(
    db: &Db,
    id: Option<&str>,
    name: Option<&str>,
    create: bool,
) -> ApiResult<Object> {
    match (id, name) {
        (Some(id), _) => require(db, Kind::Project, id).await,
        (None, Some(name)) if create => Ok(get_or_create_project(db, name, &Value::Null).await?.0),
        (None, Some(name)) => db
            .object_by_name(Kind::Project, ORG_ID, name)
            .await?
            .ok_or_else(|| ApiError::bad_request(format!("project {name:?} not found"))),
        (None, None) => Err(ApiError::bad_request(
            "`project_id` or `project_name` is required",
        )),
    }
}

pub async fn create(
    State(db): State<Db>,
    Path(kind): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let object = match parse_kind(&kind)? {
        Kind::Project => {
            get_or_create_project(&db, required(&body, "name"), &body)
                .await?
                .0
        }
        Kind::Experiment => {
            let project = require(&db, Kind::Project, required(&body, "project_id")).await?;
            let ensure_new = body["ensure_new"].as_bool().unwrap_or(false);
            create_experiment(&db, &project, str_field(&body, "name"), ensure_new, &body).await?
        }
        Kind::Dataset => {
            let project = require(&db, Kind::Project, required(&body, "project_id")).await?;
            get_or_create_dataset(&db, &project, required(&body, "name"), &body)
                .await?
                .0
        }
    };
    Ok(Json(object.data))
}

pub async fn get(
    State(db): State<Db>,
    Path((kind, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    Ok(Json(require(&db, parse_kind(&kind)?, &id).await?.data))
}

pub async fn list(
    State(db): State<Db>,
    Path(kind): Path<String>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<Value>> {
    let kind = parse_kind(&kind)?;
    let params = Params::parse(query.as_deref());
    let ids: Vec<&str> = params.all("ids").chain(params.all("id")).collect();
    let name = params.get(&format!("{}_name", kind.as_str()));
    let project_id = match (kind, params.get("project_id"), params.get("project_name")) {
        (Kind::Project, ..) | (_, None, None) => None,
        (_, Some(id), _) => Some(id.to_owned()),
        (_, None, Some(name)) => match db.object_by_name(Kind::Project, ORG_ID, name).await? {
            Some(project) => Some(project.id().to_owned()),
            None => return Ok(Json(json!({"objects": []}))),
        },
    };
    let objects: Vec<Value> = db
        .objects(kind)
        .await?
        .into_iter()
        .filter(|o| ids.is_empty() || ids.contains(&o.id()))
        .filter(|o| name.is_none_or(|n| o.name() == n))
        .filter(|o| {
            project_id
                .as_deref()
                .is_none_or(|p| o.data["project_id"] == p)
        })
        .map(|o| o.data)
        .collect();
    Ok(Json(json!({"objects": paginate(objects, &params)})))
}

/// Apply `starting_after`, `ending_before` and `limit` to a newest-first list.
pub fn paginate(mut objects: Vec<Value>, params: &Params) -> Vec<Value> {
    let position = |objects: &[Value], id: &str| objects.iter().position(|o| o["id"] == id);
    if let Some(after) = params.get("starting_after") {
        let at = position(&objects, after).map_or(objects.len(), |i| i + 1);
        objects.drain(..at);
    }
    if let Some(before) = params.get("ending_before") {
        let at = position(&objects, before).unwrap_or(0);
        objects.truncate(at);
        if let Some(limit) = params.number("limit") {
            objects.drain(..objects.len().saturating_sub(limit));
        }
    }
    if let Some(limit) = params.number("limit") {
        objects.truncate(limit);
    }
    objects
}

/// Fields that are never null in API responses.
const NOT_NULL: [&str; 2] = ["name", "public"];

/// Fields a PATCH may not change.
const IMMUTABLE: [&str; 6] = [
    "id",
    "org_id",
    "project_id",
    "created",
    "deleted_at",
    "user_id",
];

pub async fn patch(
    State(db): State<Db>,
    Path((kind, id)): Path<(String, String)>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let kind = parse_kind(&kind)?;
    let mut object = require(&db, kind, &id).await?;
    // The body was validated against the spec (an object); an empty body is a no-op.
    let fields = body.as_object().cloned().unwrap_or_default();
    let _guard = db.write_lock().await;
    if let Some(name) = fields.get("name").and_then(Value::as_str) {
        let parent = object.data["project_id"].as_str().unwrap_or(ORG_ID);
        if db
            .object_by_name(kind, parent, name)
            .await?
            .is_some_and(|o| o.id() != id)
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                format!("a {} named {name:?} already exists", kind.as_str()),
            ));
        }
    }
    for (key, value) in fields {
        // `null` cannot clear a field the API never returns as null.
        let required = value.is_null() && NOT_NULL.contains(&key.as_str());
        if !IMMUTABLE.contains(&key.as_str()) && !required {
            object.data[key] = value;
        }
    }
    db.update_object(&object.data, false).await?;
    Ok(Json(object.data))
}

pub async fn delete(
    State(db): State<Db>,
    Path((kind, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let mut object = require(&db, parse_kind(&kind)?, &id).await?;
    object.data["deleted_at"] = json!(crate::time::now_iso());
    db.update_object(&object.data, true).await?;
    Ok(Json(object.data))
}

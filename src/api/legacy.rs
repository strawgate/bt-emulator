//! Legacy app endpoints (`/api/*`) still used by the JS SDK and older Python SDKs
//! (docs/protocol.md §3.2, §4.3, §5.2).

use axum::Json;
use axum::extract::{RawQuery, State};
use serde_json::{Value, json};

use super::extract::{JsonBody, Params};
use super::objects::{create_experiment, get_or_create_dataset, project_ref, require};
use super::summary::comparison_for;
use super::{ApiError, ApiResult};
use crate::db::Db;
use crate::db::objects::Kind;

fn project_of(body: &Value, create: bool) -> (Option<&str>, Option<&str>, bool) {
    (
        body["project_id"].as_str(),
        body["project_name"].as_str(),
        create,
    )
}

pub async fn project_register(
    State(db): State<Db>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let (id, name, create) = project_of(&body, true);
    let project = project_ref(&db, id, name, create).await?;
    Ok(Json(json!({"project": project.data})))
}

pub async fn project_get_query(
    State(db): State<Db>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<Value>> {
    let params = Params::parse(query.as_deref());
    let id = params
        .get("id")
        .ok_or_else(|| ApiError::bad_request("`id` is required"))?;
    let project = require(&db, Kind::Project, id).await?;
    // JS reads both `.name` and `.project`; serve both shapes.
    let mut body = project.data.clone();
    body["project"] = project.data;
    Ok(Json(body))
}

pub async fn project_get(State(db): State<Db>, JsonBody(body): JsonBody) -> ApiResult<Json<Value>> {
    let id = body["id"]
        .as_str()
        .ok_or_else(|| ApiError::bad_request("`id` is required"))?;
    Ok(Json(json!([require(&db, Kind::Project, id).await?.data])))
}

pub async fn experiment_register(
    State(db): State<Db>,
    JsonBody(mut body): JsonBody,
) -> ApiResult<Json<Value>> {
    let (id, name, create) = project_of(&body, true);
    let project = project_ref(&db, id, name, create).await?;
    if let Some(base_name) = body["base_experiment"].as_str() {
        let base = db
            .object_by_name(Kind::Experiment, project.id(), base_name)
            .await?;
        let base = base.ok_or_else(|| {
            ApiError::bad_request(format!("base experiment {base_name:?} not found"))
        })?;
        body["base_exp_id"] = json!(base.id());
    }
    let update = body["update"].as_bool().unwrap_or(false);
    let exp = create_experiment(
        &db,
        &project,
        body["experiment_name"].as_str(),
        !update,
        &body,
    )
    .await?;
    Ok(Json(
        json!({"project": project.data, "experiment": exp.data}),
    ))
}

pub async fn experiment_get(
    State(db): State<Db>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let (id, name, _) = project_of(&body, false);
    let project = project_ref(&db, id, name, false).await?;
    let name = body["experiment_name"]
        .as_str()
        .ok_or_else(|| ApiError::bad_request("`experiment_name` is required"))?;
    let found = db
        .object_by_name(Kind::Experiment, project.id(), name)
        .await?;
    Ok(Json(json!(
        found.into_iter().map(|e| e.data).collect::<Vec<_>>()
    )))
}

pub async fn base_experiment(
    State(db): State<Db>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let id = body["id"]
        .as_str()
        .ok_or_else(|| ApiError::bad_request("`id` is required"))?;
    let exp = require(&db, Kind::Experiment, id).await?;
    // No base is a 400: Python maps it to None, and JS crashes on a 200 with null.
    let base = comparison_for(&db, &exp, None)
        .await?
        .ok_or_else(|| ApiError::bad_request("no base experiment"))?;
    Ok(Json(json!({
        "id": exp.id(), "project_id": exp.data["project_id"], "name": exp.name(),
        "base_exp_id": base.id(), "base_exp_name": base.name(),
    })))
}

pub async fn dataset_register(
    State(db): State<Db>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let (id, name, create) = project_of(&body, true);
    let project = project_ref(&db, id, name, create).await?;
    let name = body["dataset_name"]
        .as_str()
        .ok_or_else(|| ApiError::bad_request("`dataset_name` is required"))?;
    let (dataset, found_existing) = get_or_create_dataset(&db, &project, name, &body).await?;
    Ok(Json(
        json!({"project": project.data, "dataset": dataset.data, "found_existing": found_existing}),
    ))
}

pub async fn object_info(State(db): State<Db>, JsonBody(body): JsonBody) -> ApiResult<Json<Value>> {
    let kind = match body["object_type"].as_str() {
        Some("experiment") => Kind::Experiment,
        Some("dataset") => Kind::Dataset,
        other => {
            return Err(ApiError::bad_request(format!(
                "unsupported object_type {other:?}"
            )));
        }
    };
    let mut out = Vec::new();
    for id in body["object_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Some(object) = db.object(kind, id).await? {
            let project = require(
                &db,
                Kind::Project,
                object.data["project_id"].as_str().unwrap_or_default(),
            )
            .await?;
            out.push(json!({"object_id": id, "object_name": object.name(),
                            "parent_cols": {"project": {"id": project.id(), "name": project.name()}}}));
        }
    }
    Ok(Json(json!(out)))
}

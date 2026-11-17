//! Experiment and dataset summaries (docs/protocol.md §8).

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use serde_json::{Value, json};

use super::extract::{Params, base_url};
use super::objects::require;
use super::{ApiError, ApiResult, ORG_NAME};
use crate::db::Db;
use crate::db::objects::{Kind, Object};

/// The experiment to compare against: explicit, else the stored base experiment,
/// else the most recent earlier experiment in the same project.
pub async fn comparison_for(
    db: &Db,
    exp: &Object,
    explicit: Option<&str>,
) -> ApiResult<Option<Object>> {
    if let Some(id) = explicit {
        return Ok(Some(require(db, Kind::Experiment, id).await?));
    }
    if let Some(base) = exp.data["base_exp_id"].as_str() {
        if let Some(base) = db.object(Kind::Experiment, base).await? {
            return Ok(Some(base));
        }
    }
    Ok(db
        .objects(Kind::Experiment)
        .await?
        .into_iter()
        .find(|o| o.data["project_id"] == exp.data["project_id"] && o.seq < exp.seq))
}

async fn rows(db: &Db, exp: &Object) -> ApiResult<Vec<Value>> {
    let events = db.all_rows("experiment", exp.id()).await?;
    Ok(events.into_iter().map(|e| Value::Object(e.data)).collect())
}

async fn scores_and_metrics(
    db: &Db,
    exp: &Object,
    comparison: Option<&Object>,
) -> ApiResult<(Value, Value)> {
    let current = rows(db, exp).await?;
    let baseline = match comparison {
        Some(c) => Some(rows(db, c).await?),
        None => None,
    };
    let (scores, metrics) = crate::summary::summarize(&current, baseline.as_deref());
    Ok((Value::Object(scores), Value::Object(metrics)))
}

fn project_url(headers: &HeaderMap, project: &Object) -> String {
    format!("{}/app/{ORG_NAME}/p/{}", base_url(headers), project.name())
}

pub async fn experiment(
    State(db): State<Db>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let params = Params::parse(query.as_deref());
    let exp = require(&db, Kind::Experiment, &id).await?;
    let project = require(
        &db,
        Kind::Project,
        exp.data["project_id"].as_str().unwrap_or_default(),
    )
    .await?;
    let comparison = comparison_for(&db, &exp, params.get("comparison_experiment_id")).await?;
    let project_url = project_url(&headers, &project);
    let mut body = json!({
        "project_name": project.name(),
        "project_url": project_url,
        "experiment_name": exp.name(),
        "experiment_url": format!("{project_url}/experiments/{}", exp.name()),
        "comparison_experiment_name": comparison.as_ref().map(Object::name),
    });
    if params.get("summarize_scores") != Some("false") {
        let (scores, metrics) = scores_and_metrics(&db, &exp, comparison.as_ref()).await?;
        body["scores"] = scores;
        body["metrics"] = metrics;
    }
    Ok(Json(body))
}

/// Legacy comparison endpoint used by the JS SDK.
pub async fn experiment_comparison(
    State(db): State<Db>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<Value>> {
    let params = Params::parse(query.as_deref());
    let id = params
        .get("experiment_id")
        .ok_or_else(|| ApiError::bad_request("`experiment_id` is required"))?;
    let exp = require(&db, Kind::Experiment, id).await?;
    let comparison = comparison_for(&db, &exp, params.get("base_experiment_id")).await?;
    let (scores, metrics) = scores_and_metrics(&db, &exp, comparison.as_ref()).await?;
    Ok(Json(json!({"scores": scores, "metrics": metrics})))
}

async fn total_records(db: &Db, dataset: &Object) -> ApiResult<usize> {
    Ok(db.all_rows("dataset", dataset.id()).await?.len())
}

pub async fn dataset(
    State(db): State<Db>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let dataset = require(&db, Kind::Dataset, &id).await?;
    let project = require(
        &db,
        Kind::Project,
        dataset.data["project_id"].as_str().unwrap_or_default(),
    )
    .await?;
    let project_url = project_url(&headers, &project);
    Ok(Json(json!({
        "project_name": project.name(),
        "project_url": project_url,
        "dataset_name": dataset.name(),
        "dataset_url": format!("{project_url}/datasets/{}", dataset.name()),
        "data_summary": {"total_records": total_records(&db, &dataset).await?},
    })))
}

/// Legacy dataset summary used by the JS SDK.
pub async fn dataset_summary(
    State(db): State<Db>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<Value>> {
    let params = Params::parse(query.as_deref());
    let id = params
        .get("dataset_id")
        .ok_or_else(|| ApiError::bad_request("`dataset_id` is required"))?;
    let dataset = require(&db, Kind::Dataset, id).await?;
    Ok(Json(
        json!({"total_records": total_records(&db, &dataset).await?}),
    ))
}

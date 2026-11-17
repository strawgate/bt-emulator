//! `braintrust push`: `POST /function/code` hands out an upload URL for a code
//! bundle, and `POST /insert-functions` creates or updates many functions at once.
//! Bundles are stored but cannot be run (docs/protocol.md §21).

use std::collections::HashSet;

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use serde_json::{Value, json};

use super::attachments::signed_upload;
use super::extract::JsonBody;
use super::functions::{by_slug, upsert};
use super::objects::require_parent;
use super::{ApiError, ApiResult};
use crate::db::Db;

pub async fn code(
    State(db): State<Db>,
    headers: HeaderMap,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    if !body["runtime_context"].is_object() {
        return Err(ApiError::bad_request("`runtime_context` is required"));
    }
    let bundle_id = uuid::Uuid::new_v4().to_string();
    let url = signed_upload(
        &db,
        &headers,
        &bundle_id,
        "bundle",
        "application/octet-stream",
    )
    .await?;
    Ok(Json(json!({"url": url, "bundleId": bundle_id})))
}

/// What a pushed function does when its project already has one with its slug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IfExists {
    /// Append a new version.
    Replace,
    /// Keep the existing function unchanged.
    Ignore,
    /// Fail the whole batch with 400 (checked before anything is written).
    Error,
}

fn if_exists(f: &Value) -> ApiResult<IfExists> {
    match f["if_exists"].as_str() {
        None | Some("error") => Ok(IfExists::Error),
        Some("replace") => Ok(IfExists::Replace),
        Some("ignore") => Ok(IfExists::Ignore),
        Some(other) => Err(ApiError::bad_request(format!(
            "invalid if_exists {other:?}: expected error, replace or ignore"
        ))),
    }
}

pub async fn insert_functions(
    State(db): State<Db>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let functions = body["functions"]
        .as_array()
        .ok_or_else(|| ApiError::bad_request("`functions` must be an array"))?;
    // Check every definition, and every slug that must be new, before writing any;
    // hold the lock throughout so no other write can make the checks stale.
    let _guard = db.write_lock().await;
    let mut plans = Vec::with_capacity(functions.len());
    let mut slugs = HashSet::new();
    for (i, f) in functions.iter().enumerate() {
        super::validate::check("CreateFunction", f, &format!("functions/{i}"))
            .map_err(ApiError::bad_request)?;
        // The schema check guarantees an object with a project id and a slug.
        let mut stored = f.as_object().cloned().unwrap_or_default();
        stored.remove("if_exists");
        let if_exists = if_exists(f)?;
        let project_id = f["project_id"].as_str().unwrap_or_default();
        let slug = f["slug"].as_str().unwrap_or_default();
        let first = slugs.insert((project_id, slug));
        if if_exists == IfExists::Error {
            let project = require_parent(&db, project_id).await?;
            let exists = by_slug(&db, project.id(), slug, None).await?;
            if exists.is_some() || !first {
                return Err(ApiError::bad_request(format!(
                    "functions/{i}: a function with slug {slug:?} already exists in this project"
                )));
            }
        }
        plans.push((Value::Object(stored), if_exists));
    }
    let mut out = Vec::with_capacity(plans.len());
    let mut xact_id = 0;
    for (f, if_exists) in plans {
        let project_id = f["project_id"].as_str().unwrap_or_default();
        let written = upsert(
            &db,
            project_id,
            &f,
            None,
            if_exists == IfExists::Ignore,
            &[],
        )
        .await?;
        xact_id = xact_id.max(written.xact);
        out.push(json!({
            "id": written.data.get("id"),
            "project_id": written.data.get("project_id"),
            "slug": written.data.get("slug"),
            "found_existing": written.found_existing,
        }));
    }
    Ok(Json(json!({
        "functions": out,
        "status": "success",
        "xact_id": xact_id.to_string(),
    })))
}

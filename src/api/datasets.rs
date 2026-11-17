//! Dataset versions: restore (`/v1/dataset/{id}/restore[/preview]`), the JS SDK's
//! snapshot endpoints (`/api/dataset_snapshot/*`), and `GET /xact-id`, the newest
//! version, which snapshots pin. See docs/protocol.md §22.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde_json::{Map, Value, json};

use super::extract::JsonBody;
use super::objects::require;
use super::resources::{build, find, remove};
use super::rows::parse_version;
use super::{ApiError, ApiResult};
use crate::db::Db;
use crate::db::objects::Kind;

/// The spec's limit on how many rows one restore may change.
const MAX_RESTORE_ROWS: usize = 20_000;

const SNAPSHOT: &str = "dataset_snapshot";

pub async fn xact_id(State(db): State<Db>) -> ApiResult<String> {
    Ok(db.latest_xact().await?.to_string())
}

type Rows = BTreeMap<String, Map<String, Value>>;

async fn rows(db: &Db, dataset_id: &str, version: Option<i64>) -> ApiResult<Rows> {
    let events = db
        .rows_page("dataset", dataset_id, version, None, usize::MAX)
        .await?;
    Ok(events
        .into_iter()
        .map(|e| {
            (
                e.field("id").as_str().unwrap_or_default().to_owned(),
                e.data,
            )
        })
        .collect())
}

/// What restoring to the requested version does: rows to write back, with their
/// data as of that version, and rows created since, to delete.
async fn plan(db: &Db, dataset_id: &str, body: &Value) -> ApiResult<(Rows, Vec<String>)> {
    require(db, Kind::Dataset, dataset_id).await?;
    let version = parse_version(&body["version"])?
        .ok_or_else(|| ApiError::bad_request("`version` is required"))?;
    let then = rows(db, dataset_id, Some(version)).await?;
    let now = rows(db, dataset_id, None).await?;
    let (restore, delete) = diff(then, &now);
    within_limit(restore.len() + delete.len(), version)?;
    Ok((restore, delete))
}

/// How to get from `now` back to `then`: rows whose data differs (or that were
/// deleted since) are written back as they were; rows created since are deleted.
fn diff(then: Rows, now: &Rows) -> (Rows, Vec<String>) {
    let delete = now
        .keys()
        .filter(|id| !then.contains_key(*id))
        .cloned()
        .collect();
    let restore = then
        .into_iter()
        .filter(|(id, data)| now.get(id) != Some(data))
        .collect();
    (restore, delete)
}

fn within_limit(changed: usize, version: i64) -> ApiResult<()> {
    if changed > MAX_RESTORE_ROWS {
        return Err(ApiError::bad_request(format!(
            "more than {MAX_RESTORE_ROWS} rows changed since version {version}"
        )));
    }
    Ok(())
}

pub async fn restore_preview(
    State(db): State<Db>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let (restore, delete) = plan(&db, &id, &body).await?;
    Ok(Json(
        json!({"rows_to_restore": restore.len(), "rows_to_delete": delete.len()}),
    ))
}

/// Revert changed rows and delete newer ones, as one new transaction.
pub async fn restore(
    State(db): State<Db>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let _guard = db.write_lock().await;
    let (restore, delete) = plan(&db, &id, &body).await?;
    if restore.is_empty() && delete.is_empty() {
        return Ok(Json(
            json!({"xact_id": null, "rows_restored": 0, "rows_deleted": 0}),
        ));
    }
    let mut tx = db.begin().await?;
    let xact = Db::next_xact(&mut tx).await?;
    let writes = restore
        .iter()
        .map(|(row, data)| (row, Some(data)))
        .chain(delete.iter().map(|row| (row, None)));
    for (seq, (row, data)) in writes.enumerate() {
        Db::put_row(&mut tx, "dataset", &id, row, xact, seq as i64, data).await?;
    }
    tx.commit().await.map_err(crate::error::Error::from)?;
    Ok(Json(json!({
        "xact_id": xact.to_string(),
        "rows_restored": restore.len(),
        "rows_deleted": delete.len(),
    })))
}

/// `POST /api/dataset_snapshot/register {dataset_id, dataset_snapshot_name,
/// description, xact_id, update}`: create a named snapshot, or return the existing
/// one (updated to the new version when `update`).
pub async fn snapshot_register(
    State(db): State<Db>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let dataset_id = body["dataset_id"].as_str().unwrap_or_default();
    require(&db, Kind::Dataset, dataset_id).await?;
    let name = body["dataset_snapshot_name"]
        .as_str()
        .ok_or_else(|| ApiError::bad_request("`dataset_snapshot_name` is required"))?;
    if !body["xact_id"].is_string() {
        return Err(ApiError::bad_request("`xact_id` is required"));
    }
    let fields = json!({"dataset_id": dataset_id, "name": name,
        "description": body["description"], "xact_id": body["xact_id"]});
    let _guard = db.write_lock().await;
    let (snapshot, found_existing) = match db.object_by_name_of(SNAPSHOT, dataset_id, name).await? {
        Some(existing) if body["update"] == true => {
            let data = build(SNAPSHOT, &fields, Some(&existing.data));
            db.update_object(&data, false).await?;
            (data, true)
        }
        Some(existing) => (existing.data, true),
        None => {
            let data = build(SNAPSHOT, &fields, None);
            (
                db.insert_object_of(SNAPSHOT, dataset_id, data).await?.data,
                false,
            )
        }
    };
    Ok(Json(
        json!({"dataset_snapshot": snapshot, "found_existing": found_existing}),
    ))
}

/// `POST /api/dataset_snapshot/get {dataset_id, name?, xact_id?}`: matching
/// snapshots, newest first.
pub async fn snapshot_get(
    State(db): State<Db>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let matches = |o: &Value, key: &str| body[key].is_null() || o[key] == body[key];
    let snapshots: Vec<Value> = db
        .objects_of(SNAPSHOT)
        .await?
        .into_iter()
        .map(|o| o.data)
        .filter(|o| matches(o, "dataset_id") && matches(o, "name") && matches(o, "xact_id"))
        .collect();
    Ok(Json(Value::Array(snapshots)))
}

/// `POST /api/dataset_snapshot/patch_id {id, name?, description?}`.
pub async fn snapshot_patch(
    State(db): State<Db>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let id = body["id"].as_str().unwrap_or_default();
    let _guard = db.write_lock().await;
    let mut data = find(&db, SNAPSHOT, id).await?.data;
    // Names are unique per dataset, as with `PATCH /v1/dataset_snapshot/{id}`.
    if let Some(name) = body["name"].as_str() {
        let dataset_id = data["dataset_id"].as_str().unwrap_or_default();
        let taken = db.object_by_name_of(SNAPSHOT, dataset_id, name).await?;
        if taken.is_some_and(|o| o.id() != id) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                format!("a dataset_snapshot named {name:?} already exists"),
            ));
        }
    }
    for key in ["name", "description"] {
        if let Some(value) = body.get(key).filter(|v| v.is_string()) {
            data[key] = value.clone();
        }
    }
    db.update_object(&data, false).await?;
    Ok(Json(data))
}

/// `GET /environment-object/{object_type}/{id}/{environment}`: the version of an
/// object an environment serves, which `init_dataset(environment=)` loads. Nothing
/// public assigns datasets to environments, so this is refused (docs/protocol.md §5.2).
pub async fn environment_object() -> ApiResult<Json<Value>> {
    Err(ApiError::bad_request(
        "loading a dataset by environment is not supported: bt-emulator has no way to assign \
         datasets to environments; load it by version instead",
    ))
}

/// `POST /api/dataset_snapshot/delete_id {id}`.
pub async fn snapshot_delete(
    State(db): State<Db>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let id = body["id"].as_str().unwrap_or_default();
    let data = find(&db, SNAPSHOT, id).await?.data;
    remove(&db, data).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(entries: &[(&str, i64)]) -> Rows {
        entries
            .iter()
            .map(|(id, v)| {
                let data = json!({"id": id, "v": v}).as_object().unwrap().clone();
                ((*id).to_owned(), data)
            })
            .collect()
    }

    #[test]
    fn restoring_reverts_changes_and_drops_newer_rows() {
        // then: a=1, b=1, c=1. Since: a unchanged, b edited, c deleted, d created.
        let then = rows(&[("a", 1), ("b", 1), ("c", 1)]);
        let now = rows(&[("a", 1), ("b", 2), ("d", 1)]);
        let (restore, delete) = diff(then, &now);
        assert_eq!(restore, rows(&[("b", 1), ("c", 1)]));
        assert_eq!(delete, ["d"]);
    }

    #[test]
    fn restoring_the_current_version_changes_nothing() {
        let now = rows(&[("a", 1), ("b", 2)]);
        let (restore, delete) = diff(now.clone(), &now);
        assert!(restore.is_empty() && delete.is_empty());
        let (restore, delete) = diff(Rows::new(), &Rows::new());
        assert!(restore.is_empty() && delete.is_empty());
    }

    #[test]
    fn restoring_before_the_first_row_deletes_everything() {
        let (restore, delete) = diff(Rows::new(), &rows(&[("a", 1), ("b", 1)]));
        assert!(restore.is_empty());
        assert_eq!(delete, ["a", "b"]);
    }

    #[test]
    fn restores_are_limited_to_20000_rows() {
        assert!(within_limit(MAX_RESTORE_ROWS, 1).is_ok());
        let err = within_limit(MAX_RESTORE_ROWS + 1, 7).unwrap_err();
        assert!(
            err.body["error"]
                .as_str()
                .unwrap()
                .contains("since version 7")
        );
    }
}

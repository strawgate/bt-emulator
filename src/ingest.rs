//! The row write path shared by `/logs3` and the REST insert/feedback endpoints.
//!
//! Every request is one transaction with one `_xact_id`. Per row: `_object_delete`
//! writes a tombstone, `_is_merge` deep-merges into the current row (or inserts if
//! there is none), anything else replaces the row. See docs/protocol.md §6.

use serde_json::{Map, Value, json};

use crate::db::Db;
use crate::error::Error;
use crate::merge::{merge_paths, merge_row};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub object_type: &'static str,
    pub object_id: String,
}

#[derive(Debug)]
pub struct Written {
    pub ids: Vec<String>,
    pub xact_id: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Db(#[from] Error),
}

/// Addressing and control fields that are never stored in the row itself.
const STRIP: [&str; 13] = [
    "experiment_id",
    "dataset_id",
    "project_id",
    "log_id",
    "org_id",
    "prompt_session_id",
    "_xact_id",
    "_parent_id",
    "_array_delete",
    "_async_scoring_control",
    "_skip_async_scoring",
    "_pagination_key",
    "is_root",
];

/// The object a `/logs3` row addresses.
pub fn target_of(row: &Map<String, Value>) -> Option<Target> {
    let field = |k: &str| row.get(k).and_then(Value::as_str).map(str::to_owned);
    if let Some(id) = field("experiment_id") {
        Some(Target {
            object_type: "experiment",
            object_id: id,
        })
    } else if let Some(id) = field("dataset_id") {
        Some(Target {
            object_type: "dataset",
            object_id: id,
        })
    } else if let (Some(id), Some("g")) = (
        field("project_id"),
        row.get("log_id").and_then(Value::as_str),
    ) {
        Some(Target {
            object_type: "project_logs",
            object_id: id,
        })
    } else {
        None
    }
}

pub async fn write(
    db: &Db,
    rows: Vec<Value>,
    default: Option<&Target>,
) -> Result<Written, IngestError> {
    let mut parsed = Vec::with_capacity(rows.len());
    for row in rows {
        let Value::Object(row) = row else {
            return Err(IngestError::Invalid(
                "each row must be a JSON object".into(),
            ));
        };
        let target = target_of(&row)
            .or_else(|| default.cloned())
            .ok_or_else(|| {
                IngestError::Invalid(
                    "row must set experiment_id, dataset_id, or project_id with log_id \"g\""
                        .into(),
                )
            })?;
        parsed.push((target, row));
    }

    let _guard = db.write_lock().await;
    let mut tx = db.begin().await?;
    let xact = Db::next_xact(&mut tx).await?;
    let mut ids = Vec::with_capacity(parsed.len());
    for (seq, (target, mut row)) in parsed.into_iter().enumerate() {
        let id = match row.remove("id") {
            Some(Value::String(id)) => id,
            _ => uuid::Uuid::new_v4().to_string(),
        };
        apply(&mut tx, &target, &id, xact, seq as i64, row).await?;
        ids.push(id);
    }
    tx.commit().await.map_err(Error::from)?;
    Ok(Written { ids, xact_id: xact })
}

fn take_flag(row: &mut Map<String, Value>, key: &str) -> bool {
    matches!(row.remove(key), Some(Value::Bool(true)))
}

async fn apply(
    conn: &mut sqlx::AnyConnection,
    target: &Target,
    id: &str,
    xact: i64,
    seq: i64,
    mut row: Map<String, Value>,
) -> Result<(), Error> {
    let (ty, oid) = (target.object_type, target.object_id.as_str());
    let is_merge = take_flag(&mut row, "_is_merge");
    let delete = take_flag(&mut row, "_object_delete");
    let paths = merge_paths(row.remove("_merge_paths").as_ref());
    let source = match row.remove("_audit_source") {
        Some(Value::String(s)) => s,
        _ => "api".to_owned(),
    };
    let metadata = match row.remove("_audit_metadata") {
        Some(m @ Value::Object(_)) => m,
        _ => json!({}),
    };
    for key in STRIP {
        row.remove(key);
    }

    if delete {
        return Db::put_row(conn, ty, oid, id, xact, seq, None).await;
    }

    // A feedback comment is its own row that points at the row it annotates.
    let origin_id = row
        .get("origin")
        .and_then(|o| o["id"].as_str())
        .map(str::to_owned);
    if let (Some(comment), Some(origin_id)) = (row.get("comment").cloned(), origin_id) {
        if let Some(mut annotated) = Db::current_row(conn, ty, oid, &origin_id).await? {
            let entry = json!({"id": id, "created": crate::time::now_iso(), "comment": comment,
                               "source": source, "metadata": metadata});
            push(&mut annotated, "comments", entry);
            return Db::put_row(conn, ty, oid, &origin_id, xact, seq, Some(&annotated)).await;
        }
    }

    let existing = Db::current_row(conn, ty, oid, id).await?;
    let (mut data, action) = match existing {
        Some(mut current) if is_merge => {
            row.remove("created");
            merge_row(&mut current, row, &paths);
            (current, "merge")
        }
        Some(previous) => {
            // A replace keeps the row's identity and history.
            for key in ["created", "audit_data", "comments"] {
                if let (false, Some(v)) = (row.contains_key(key), previous.get(key)) {
                    row.insert(key.to_owned(), v.clone());
                }
            }
            (row, "upsert")
        }
        None => (row, "upsert"),
    };
    data.insert("id".into(), json!(id));
    let created = data
        .get("created")
        .and_then(Value::as_str)
        .map_or_else(crate::time::now_iso, crate::time::normalize);
    data.insert("created".into(), json!(created));
    if data.get("span_id").is_none_or(Value::is_null) {
        // Datasets get a random span id; spans written via REST default to their row id.
        let span = if ty == "dataset" {
            uuid::Uuid::new_v4().to_string()
        } else {
            id.to_owned()
        };
        data.insert("span_id".into(), json!(span));
        if data.get("root_span_id").is_none_or(Value::is_null) {
            data.insert("root_span_id".into(), json!(span));
        }
    }
    if ty != "dataset" {
        // The real server stamps who wrote a span.
        let attrs = data.entry("span_attributes").or_insert_with(|| json!({}));
        if let Value::Object(attrs) = attrs {
            attrs
                .entry("created_by_user_id")
                .or_insert(json!(crate::api::USER_ID));
            attrs
                .entry("created_by_api_key_id")
                .or_insert(json!(crate::api::API_KEY_ID));
        }
    }
    let change = match action {
        "merge" => json!({"action": "merge", "from": null, "path": [], "to": null}),
        _ => json!({"action": "upsert"}),
    };
    let audit = json!({"_xact_id": xact.to_string(), "audit_data": change, "metadata": metadata, "source": source});
    push(&mut data, "audit_data", audit);
    Db::put_row(conn, ty, oid, id, xact, seq, Some(&data)).await
}

fn push(row: &mut Map<String, Value>, key: &str, entry: Value) {
    match row.get_mut(key) {
        Some(Value::Array(list)) => list.push(entry),
        _ => {
            row.insert(key.to_owned(), json!([entry]));
        }
    }
}

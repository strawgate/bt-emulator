//! Row ingestion and reads: `/logs3`, `/v1/{type}/{id}/{insert,fetch,feedback}`,
//! and `/btql`. See docs/protocol.md §6, §7 and §10.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use super::extract::{JsonBody, Params};
use super::objects::require;
use super::{ApiError, ApiResult};
use crate::db::Db;
use crate::db::events::{Event, Position};
use crate::db::objects::Kind;
use crate::ingest::{self, Target};

const DEFAULT_LIMIT: usize = 1000;

/// Columns every event carries (null when unset), per docs/protocol.md §7.1.
const COMMON_COLUMNS: [&str; 17] = [
    "_pagination_key",
    "_xact_id",
    "audit_data",
    "classifications",
    "comments",
    "created",
    "expected",
    "facets",
    "id",
    "input",
    "is_root",
    "metadata",
    "origin",
    "project_id",
    "root_span_id",
    "span_id",
    "tags",
];
const SPAN_COLUMNS: [&str; 7] = [
    "context",
    "error",
    "metrics",
    "output",
    "scores",
    "span_attributes",
    "span_parents",
];

/// A row-bearing object and the project it belongs to.
struct Source {
    target: Target,
    project_id: String,
}

async fn source(db: &Db, object_type: &str, object_id: &str) -> ApiResult<Source> {
    let (object_type, project_id) = match object_type {
        "experiment" => (
            "experiment",
            require(db, Kind::Experiment, object_id).await?.data["project_id"].clone(),
        ),
        "dataset" => (
            "dataset",
            require(db, Kind::Dataset, object_id).await?.data["project_id"].clone(),
        ),
        "project_logs" => (
            "project_logs",
            json!(require(db, Kind::Project, object_id).await?.id()),
        ),
        other => {
            return Err(ApiError::not_found(format!(
                "bt-emulator does not implement {other} rows"
            )));
        }
    };
    let target = Target {
        object_type,
        object_id: object_id.to_owned(),
    };
    Ok(Source {
        target,
        project_id: project_id.as_str().unwrap_or_default().to_owned(),
    })
}

fn shape(src: &Source, event: Event) -> Value {
    let mut row = Map::new();
    let ty = src.target.object_type;
    let columns = COMMON_COLUMNS.iter().chain(if ty == "dataset" {
        &[][..]
    } else {
        &SPAN_COLUMNS[..]
    });
    for column in columns {
        row.insert((*column).to_owned(), Value::Null);
    }
    match ty {
        "experiment" => row.insert("experiment_id".into(), json!(src.target.object_id)),
        "dataset" => row.insert("dataset_id".into(), json!(src.target.object_id)),
        _ => row.insert("log_id".into(), json!("g")),
    };
    row.extend(event.data);
    let is_root = row
        .get("span_parents")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty);
    row.insert("is_root".into(), json!(is_root));
    row.insert("project_id".into(), json!(src.project_id));
    row.insert("_xact_id".into(), json!(event.xact_id.to_string()));
    row.insert(
        "_pagination_key".into(),
        json!(format!("p{:020}", event.xact_id)),
    );
    Value::Object(row)
}

fn encode_cursor((xact, seq): Position) -> String {
    format!("{xact}.{seq}")
}

fn decode_cursor(cursor: &str) -> ApiResult<Position> {
    let parse = |s: &str| s.parse::<i64>().ok();
    match cursor.split_once('.') {
        Some((x, s)) if parse(x).is_some() && parse(s).is_some() => {
            Ok((parse(x).unwrap(), parse(s).unwrap()))
        }
        _ => Err(ApiError::bad_request(format!("invalid cursor {cursor:?}"))),
    }
}

fn parse_version(v: &Value) -> ApiResult<Option<i64>> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) if n.is_i64() => Ok(n.as_i64()),
        Value::String(s) if s.parse::<i64>().is_ok() => Ok(s.parse().ok()),
        other => Err(ApiError::bad_request(format!("invalid version {other}"))),
    }
}

/// One page of rows, newest first. A non-empty page always carries a cursor and an
/// empty page never does, matching the real API (the SDKs page until no cursor).
async fn page(
    db: &Db,
    src: &Source,
    version: Option<i64>,
    cursor: Option<&str>,
    limit: usize,
    filter: Option<&Value>,
) -> ApiResult<(Vec<Value>, Option<String>)> {
    let limit = limit.max(1);
    let mut after = cursor.map(decode_cursor).transpose()?;
    let (mut out, mut last) = (Vec::new(), None);
    let (ty, id) = (src.target.object_type, src.target.object_id.as_str());
    'scan: loop {
        let batch = db.rows_page(ty, id, version, after, limit).await?;
        let exhausted = batch.len() < limit;
        for event in batch {
            after = Some((event.xact_id, event.seq));
            let row = shape(src, event);
            if filter
                .map_or(Ok(true), |f| crate::btql::matches(f, &row))
                .map_err(ApiError::bad_request)?
            {
                out.push(row);
                last = after;
                if out.len() == limit {
                    break 'scan;
                }
            }
        }
        if exhausted {
            break;
        }
    }
    Ok((out, last.map(encode_cursor)))
}

/// The real API returns the cursor both in the body and in an `x-bt-cursor` header.
fn with_cursor(mut body: Value, cursor: Option<String>) -> Response {
    let Some(cursor) = cursor else {
        return Json(body).into_response();
    };
    body["cursor"] = json!(cursor);
    cursor_header(Json(body).into_response(), &cursor)
}

fn cursor_header(mut resp: Response, cursor: &str) -> Response {
    resp.headers_mut().insert(
        "x-bt-cursor",
        HeaderValue::from_str(cursor).expect("cursor is ASCII"),
    );
    resp
}

pub async fn logs3(State(db): State<Db>, JsonBody(body): JsonBody) -> ApiResult<Json<Value>> {
    let rows = match body["rows"].clone() {
        Value::Array(rows) => rows,
        Value::Object(o) if o.get("type").is_some_and(|t| t == "logs3_overflow") => {
            return Err(ApiError::bad_request(
                "logs3 overflow uploads are not supported",
            ));
        }
        _ => return Err(ApiError::bad_request("`rows` must be an array")),
    };
    let written = ingest::write(&db, rows, None).await?;
    Ok(Json(
        json!({"ids": written.ids, "xact_id": written.xact_id.to_string()}),
    ))
}

pub async fn insert(
    State(db): State<Db>,
    Path((ty, id)): Path<(String, String)>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let src = source(&db, &ty, &id).await?;
    let Value::Array(events) = body["events"].clone() else {
        return Err(ApiError::bad_request("`events` must be an array"));
    };
    let written = ingest::write(&db, events, Some(&src.target)).await?;
    Ok(Json(json!({"row_ids": written.ids})))
}

pub async fn feedback(
    State(db): State<Db>,
    Path((ty, id)): Path<(String, String)>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let src = source(&db, &ty, &id).await?;
    let Value::Array(items) = body["feedback"].clone() else {
        return Err(ApiError::bad_request("`feedback` must be an array"));
    };
    let mut rows = Vec::new();
    for item in items {
        let target_id = item["id"]
            .as_str()
            .ok_or_else(|| ApiError::bad_request("feedback `id` is required"))?;
        let source = item["source"].as_str().unwrap_or("external");
        let mut merge = json!({"id": target_id, "_is_merge": true, "_audit_source": source, "_audit_metadata": item["metadata"]});
        for key in ["scores", "expected", "tags", "metadata"] {
            if !item[key].is_null() {
                merge[key] = item[key].clone();
            }
        }
        rows.push(merge);
        if !item["comment"].is_null() {
            rows.push(json!({"origin": {"id": target_id}, "comment": {"text": item["comment"]}, "_audit_source": source}));
        }
    }
    ingest::write(&db, rows, Some(&src.target)).await?;
    Ok(Json(json!({"status": "success"})))
}

pub async fn fetch_post(
    State(db): State<Db>,
    Path((ty, id)): Path<(String, String)>,
    JsonBody(body): JsonBody,
) -> ApiResult<Response> {
    let src = source(&db, &ty, &id).await?;
    let limit = body["limit"].as_u64().map_or(DEFAULT_LIMIT, |l| l as usize);
    let version = parse_version(&body["version"])?;
    let (events, cursor) = page(&db, &src, version, body["cursor"].as_str(), limit, None).await?;
    Ok(with_cursor(json!({"events": events}), cursor))
}

pub async fn fetch_get(
    State(db): State<Db>,
    Path((ty, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> ApiResult<Response> {
    let src = source(&db, &ty, &id).await?;
    let params = Params::parse(query.as_deref());
    let version = parse_version(&params.get("version").map_or(Value::Null, |v| json!(v)))?;
    let limit = params.number("limit")?.unwrap_or(DEFAULT_LIMIT);
    let (events, cursor) = page(&db, &src, version, params.get("cursor"), limit, None).await?;
    Ok(with_cursor(json!({"events": events}), cursor))
}

pub async fn btql(State(db): State<Db>, JsonBody(body): JsonBody) -> ApiResult<Response> {
    let query = crate::btql::parse(&body).map_err(ApiError::bad_request)?;
    let (data, cursor) = if query.object_type == "project_prompts" {
        (Vec::new(), None)
    } else {
        let src = source(&db, &query.object_type, &query.object_id).await?;
        let version = parse_version(&body["version"])?;
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
        page(
            &db,
            &src,
            version,
            query.cursor.as_deref(),
            limit,
            query.filter.as_ref(),
        )
        .await?
    };
    if body["fmt"] == "jsonl" {
        // Rows one per line; the cursor moves to a header.
        let lines: Vec<String> = data.iter().map(Value::to_string).collect();
        let resp = (
            [(header::CONTENT_TYPE, "application/json")],
            lines.join("\n"),
        )
            .into_response();
        return Ok(match cursor {
            Some(cursor) => cursor_header(resp, &cursor),
            None => resp,
        });
    }
    let schema = json!({"type": "array", "items": {"type": "object", "properties": {}}});
    // Reads are always fully fresh: everything written is immediately visible.
    let latest = data
        .iter()
        .filter_map(|r| r["_xact_id"].as_str())
        .max()
        .unwrap_or("0")
        .to_owned();
    let body = json!({
        "data": data, "schema": schema, "warnings": [],
        "freshness_state": {"last_considered_xact_id": latest, "last_processed_xact_id": latest},
        "realtime_state": {"type": "on", "minimum_xact_id": latest, "actual_xact_id": latest, "read_bytes": 0},
    });
    Ok(with_cursor(body, cursor))
}

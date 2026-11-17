//! Attachments (docs/protocol.md §19). `POST /attachment` hands out a signed URL
//! that points back at the emulator; the SDK uploads the bytes there, reports the
//! upload status, and reads them back through `GET /attachment`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use super::extract::{JsonBody, Params, base_url};
use super::{ApiError, ApiResult};
use crate::db::Db;
use crate::db::attachments::Attachment;

fn blob_url(headers: &HeaderMap, a: &Attachment) -> String {
    format!(
        "{}/attachment/blob/{}?token={}",
        base_url(headers),
        a.key,
        a.token
    )
}

fn field<'a>(body: &'a Value, key: &str) -> ApiResult<&'a str> {
    body[key]
        .as_str()
        .ok_or_else(|| ApiError::bad_request(format!("`{key}` is required")))
}

pub async fn create(
    State(db): State<Db>,
    headers: HeaderMap,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let key = field(&body, "key")?;
    let attachment = Attachment {
        key: key.to_owned(),
        filename: field(&body, "filename")?.to_owned(),
        content_type: field(&body, "content_type")?.to_owned(),
        token: uuid::Uuid::new_v4().simple().to_string(),
        status: json!({"upload_status": "uploading"}),
        data: None,
    };
    db.put_attachment(&attachment).await?;
    // The stored row keeps the first token handed out for this key.
    let stored = db.attachment(key).await?.unwrap_or(attachment);
    Ok(Json(json!({
        "signedUrl": blob_url(&headers, &stored),
        "headers": {"Content-Type": stored.content_type, "If-None-Match": "*"},
    })))
}

pub async fn status(State(db): State<Db>, JsonBody(body): JsonBody) -> ApiResult<Json<Value>> {
    let key = field(&body, "key")?;
    if !body["status"].is_object() {
        return Err(ApiError::bad_request("`status` must be an object"));
    }
    if db.attachment(key).await?.is_none() {
        return Err(ApiError::bad_request(format!("unknown attachment {key:?}")));
    }
    db.set_attachment_status(key, &body["status"]).await?;
    Ok(Json(json!({"status": "success"})))
}

pub async fn metadata(
    State(db): State<Db>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<Value>> {
    let params = Params::parse(query.as_deref());
    if let Some(url) = params.get("url") {
        // External attachments are read from their own URL.
        return Ok(Json(
            json!({"downloadUrl": url, "status": {"upload_status": "done"}}),
        ));
    }
    let key = params
        .get("key")
        .ok_or_else(|| ApiError::bad_request("`key` or `url` is required"))?;
    let a = db
        .attachment(key)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("unknown attachment {key:?}")))?;
    Ok(Json(
        json!({"downloadUrl": blob_url(&headers, &a), "status": a.status}),
    ))
}

/// The attachment a signed URL names, if its token matches.
async fn signed(db: &Db, key: &str, query: Option<&str>) -> ApiResult<Attachment> {
    let params = Params::parse(query);
    match db.attachment(key).await? {
        Some(a) if params.get("token") == Some(a.token.as_str()) => Ok(a),
        _ => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "invalid or expired signed URL",
        )),
    }
}

pub async fn upload(
    State(db): State<Db>,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> ApiResult<StatusCode> {
    signed(&db, &key, query.as_deref()).await?;
    db.set_attachment_data(&key, &STANDARD.encode(&body))
        .await?;
    Ok(StatusCode::OK)
}

pub async fn download(
    State(db): State<Db>,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
) -> ApiResult<Response> {
    let a = signed(&db, &key, query.as_deref()).await?;
    let data = a
        .data
        .and_then(|d| STANDARD.decode(d).ok())
        .ok_or_else(|| ApiError::not_found(format!("attachment {key:?} has not been uploaded")))?;
    Ok(([(header::CONTENT_TYPE, a.content_type)], data).into_response())
}

//! HTTP surface. One router serves both the Braintrust "app" URL (`/api/*`) and
//! the "api" URL; their paths do not collide.

mod error;
mod extract;
mod legacy;
mod meta;
mod objects;
mod rows;
mod summary;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};

pub use error::{ApiError, ApiResult};

use crate::db::Db;

pub const ORG_ID: &str = "7e5a3a7c-2a5c-4d0b-9a8e-0b7e3e3f0001";
pub const ORG_NAME: &str = "bt-emulator";
pub const USER_ID: &str = "7e5a3a7c-2a5c-4d0b-9a8e-0b7e3e3f0002";
pub const API_KEY_ID: &str = "7e5a3a7c-2a5c-4d0b-9a8e-0b7e3e3f0003";
/// Maximum request body; also advertised as `logs3_payload_max_bytes`.
pub const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

pub fn router(db: Db) -> Router {
    Router::new()
        .route("/", get(root))
        // App URL
        .route("/api/apikey/login", post(meta::login))
        .route("/api/project", get(legacy::project_get_query))
        .route("/api/project/register", post(legacy::project_register))
        .route("/api/project/get", post(legacy::project_get))
        .route(
            "/api/experiment/register",
            post(legacy::experiment_register),
        )
        .route("/api/experiment/get", post(legacy::experiment_get))
        .route("/api/base_experiment/get_id", post(legacy::base_experiment))
        .route("/api/dataset/register", post(legacy::dataset_register))
        .route("/api/self/get_object_info", post(legacy::object_info))
        // API URL
        .route("/ping", get(meta::ping))
        .route("/version", get(meta::version))
        .route("/logs3", post(rows::logs3))
        .route("/btql", post(rows::btql))
        .route(
            "/experiment-comparison2",
            get(summary::experiment_comparison),
        )
        .route("/dataset-summary", get(summary::dataset_summary))
        .route("/v1/{kind}", get(objects::list).post(objects::create))
        .route(
            "/v1/{kind}/{id}",
            get(objects::get)
                .patch(objects::patch)
                .delete(objects::delete),
        )
        .route("/v1/{kind}/{id}/insert", post(rows::insert))
        .route(
            "/v1/{kind}/{id}/fetch",
            get(rows::fetch_get).post(rows::fetch_post),
        )
        .route("/v1/{kind}/{id}/feedback", post(rows::feedback))
        .route("/v1/experiment/{id}/summarize", get(summary::experiment))
        .route("/v1/dataset/{id}/summarize", get(summary::dataset))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(db)
}

async fn root() -> &'static str {
    "bt-emulator"
}

async fn not_found(uri: axum::http::Uri) -> ApiError {
    ApiError::not_found(format!("bt-emulator does not implement {}", uri.path()))
}

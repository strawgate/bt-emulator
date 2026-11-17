//! HTTP surface. One router serves both the Braintrust "app" URL (`/api/*`) and
//! the "api" URL; their paths do not collide.

mod attachments;
mod datasets;
mod error;
mod extract;
mod functions;
mod invoke;
mod legacy;
mod meta;
mod objects;
mod otel;
mod proxy;
mod push;
mod resources;
mod rows;
mod summary;
mod validate;

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

/// Server options beyond the database.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// When set, only this API key is accepted; otherwise any bearer token is.
    pub api_key: Option<String>,
    /// Where function invocations run.
    pub runtime: crate::invoke::Runtime,
    /// The organization's name (default [`ORG_NAME`]). SDKs configured with an org
    /// name (`BRAINTRUST_ORG_NAME`) refuse to log in unless it matches.
    pub org_name: Option<String>,
}

/// The configured organization name, available to handlers as an extension.
#[derive(Debug, Clone)]
pub struct OrgName(pub std::sync::Arc<str>);

/// The router with default options (any API key accepted).
pub fn router(db: Db) -> Router {
    app(db, Config::default())
}

pub fn app(db: Db, config: Config) -> Router {
    let api_key: Option<std::sync::Arc<str>> = config.api_key.map(Into::into);
    let runtime = std::sync::Arc::new(config.runtime);
    let org = OrgName(config.org_name.as_deref().unwrap_or(ORG_NAME).into());
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
        .route("/otel/v1/traces", post(otel::traces))
        .route(
            "/attachment",
            get(attachments::metadata).post(attachments::create),
        )
        .route("/attachment/status", post(attachments::status))
        .route(
            "/attachment/blob/{key}",
            get(attachments::download).put(attachments::upload),
        )
        .route(
            "/experiment-comparison2",
            get(summary::experiment_comparison),
        )
        .route("/dataset-summary", get(summary::dataset_summary))
        .route("/v1/function", functions_root())
        .route("/v1/prompt", functions_root())
        .route("/v1/function/{id}", functions_item())
        .route("/v1/function/{id}/invoke", post(invoke::by_id))
        .route("/function/invoke", post(invoke::by_ref))
        .route("/function/code", post(push::code))
        .route("/insert-functions", post(push::insert_functions))
        .route("/v1/prompt/{id}", functions_item())
        .route("/v1/proxy/{*path}", axum::routing::any(proxy::proxied))
        .route("/v1/chat/completions", post(proxy::openai))
        .route("/v1/completions", post(proxy::openai))
        .route("/v1/embeddings", post(proxy::openai))
        .route("/v1/responses", post(proxy::openai))
        .merge(resource_routes())
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
        .route("/v1/dataset/{id}/restore", post(datasets::restore))
        .route(
            "/v1/dataset/{id}/restore/preview",
            post(datasets::restore_preview),
        )
        .route("/xact-id", get(datasets::xact_id))
        .route(
            "/api/dataset_snapshot/register",
            post(datasets::snapshot_register),
        )
        .route("/api/dataset_snapshot/get", post(datasets::snapshot_get))
        .route(
            "/api/dataset_snapshot/patch_id",
            post(datasets::snapshot_patch),
        )
        .route(
            "/api/dataset_snapshot/delete_id",
            post(datasets::snapshot_delete),
        )
        .fallback(not_found)
        .layer(axum::middleware::from_fn(validate::middleware))
        .layer(axum::middleware::from_fn_with_state(
            api_key,
            validate::require_auth,
        ))
        .layer(axum::Extension(runtime))
        .layer(axum::Extension(org))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        // A bug in one handler answers 500 instead of dropping the connection.
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        .with_state(db)
}

fn resource_routes() -> Router<Db> {
    let mut router = Router::new();
    for (kind, _) in resources::KINDS {
        let mut root = get(resources::list).post(resources::create);
        // The spec has no create-or-replace for environments.
        if kind != "environment" {
            root = root.put(resources::create);
        }
        if kind == "ai_secret" {
            root = root.delete(resources::delete_by_name);
        }
        let base = resources::path(kind);
        router = router.route(&base, root).route(
            &format!("{base}/{{id}}"),
            get(resources::get)
                .patch(resources::patch)
                .delete(resources::delete),
        );
    }
    router
}

fn functions_root() -> axum::routing::MethodRouter<Db> {
    get(functions::list)
        .post(functions::create)
        .put(functions::create)
}

fn functions_item() -> axum::routing::MethodRouter<Db> {
    get(functions::get)
        .patch(functions::patch)
        .delete(functions::delete)
}

async fn root() -> &'static str {
    "bt-emulator"
}

async fn not_found(uri: axum::http::Uri) -> ApiError {
    ApiError::not_found(format!("bt-emulator does not implement {}", uri.path()))
}

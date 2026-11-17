use axum::Router;
use axum::routing::get;

use crate::db::Db;

pub fn router(db: Db) -> Router {
    Router::new().route("/", get(root)).with_state(db)
}

async fn root() -> &'static str {
    "bt-emulator"
}

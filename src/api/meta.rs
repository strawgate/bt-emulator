//! Login, org discovery and server metadata.

use axum::Json;
use axum::http::HeaderMap;
use serde_json::{Value, json};

use super::extract::base_url;
use super::{ORG_ID, ORG_NAME};

pub async fn login(headers: HeaderMap) -> Json<Value> {
    let base = base_url(&headers);
    Json(json!({"org_info": [{
        "id": ORG_ID,
        "name": ORG_NAME,
        "api_url": base,
        "proxy_url": base,
        "realtime_url": base.replacen("http", "ws", 1),
        "is_universal_api": true,
        "git_metadata": {
            "collect": "some",
            "fields": ["commit", "branch", "tag", "dirty", "author_name", "author_email", "commit_message", "commit_time"],
        },
    }]}))
}

pub async fn ping() -> Json<Value> {
    Json(json!({}))
}

pub async fn version() -> Json<Value> {
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "date_version": "bt-emulator",
        "ff_version": 0,
        "commit": "bt-emulator",
        "deployment_mode": "local",
        "deployment_type": "bt-emulator",
        "brainstore_default": "force",
        "brainstore_can_contain_row_refs": false,
        "brainstore_export_enabled": false,
        "brainstore_wal_footer_version": "v3",
        "brainstore_wal_use_efficient_format": true,
        "control_plane_telemetry": [],
        "has_logs2": true,
        "has_realtime_wal_bucket": false,
        "js": false,
        "loop_runtime_enabled": false,
        "skip_pg_config": "all",
        "universal": true,
        "use_gateway": false,
        "code_execution": false,
        // Large enough that the SDKs never use the signed-URL overflow path.
        "logs3_payload_max_bytes": super::MAX_BODY_BYTES,
    }))
}

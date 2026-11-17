use std::future::Future;

use tokio::net::TcpListener;

use crate::db::Db;

pub async fn serve(
    listener: TcpListener,
    db: Db,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, crate::api::router(db))
        .with_graceful_shutdown(shutdown)
        .await
}

/// Resolves on SIGINT or SIGTERM.
pub async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

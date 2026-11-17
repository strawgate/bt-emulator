use std::future::Future;

use tokio::net::TcpListener;

use crate::db::Db;

pub async fn serve(
    listener: TcpListener,
    db: Db,
    config: crate::api::Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, crate::api::app(db, config))
        .with_graceful_shutdown(shutdown)
        .await
}

/// Resolves on SIGINT or SIGTERM. The handlers are installed when this is called,
/// so a signal sent any time afterwards (even before the future is polled) is seen.
pub fn shutdown_signal() -> impl Future<Output = ()> + Send + 'static {
    use tokio::signal::unix::{SignalKind, signal};
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    async move {
        tokio::select! {
            _ = int.recv() => {}
            _ = term.recv() => {}
        }
    }
}

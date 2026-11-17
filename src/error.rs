/// Errors produced by the emulator.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported database url: {0} (expected sqlite: or postgres:)")]
    UnsupportedUrl(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("invalid stored JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

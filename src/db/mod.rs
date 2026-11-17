//! Storage backend. One code path for SQLite and PostgreSQL via sqlx's `Any`
//! driver; the backend is chosen at runtime from the database URL. All SQL in
//! this crate must be portable between the two (use `$N` placeholders).

use sqlx::AnyPool;
use sqlx::any::{AnyPoolOptions, install_default_drivers};

use crate::error::Error;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Sqlite,
    Postgres,
}

impl Backend {
    pub fn from_url(url: &str) -> Result<Self, Error> {
        if url.starts_with("sqlite:") {
            Ok(Self::Sqlite)
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            Ok(Self::Postgres)
        } else {
            Err(Error::UnsupportedUrl(url.to_owned()))
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Db {
    pool: AnyPool,
    backend: Backend,
}

impl Db {
    /// Connect and apply migrations.
    pub async fn connect(url: &str) -> Result<Self, Error> {
        let db = Self::connect_without_migrations(url).await?;
        db.migrate().await?;
        Ok(db)
    }

    pub async fn connect_without_migrations(url: &str) -> Result<Self, Error> {
        install_default_drivers();
        let backend = Backend::from_url(url)?;
        let (url, opts) = match backend {
            Backend::Sqlite if url.contains(":memory:") => {
                // Each in-memory connection is its own database, so keep exactly
                // one connection alive for the life of the pool.
                let opts = AnyPoolOptions::new()
                    .max_connections(1)
                    .min_connections(1)
                    .idle_timeout(None)
                    .max_lifetime(None);
                (url.to_owned(), opts)
            }
            Backend::Sqlite => {
                let sep = if url.contains('?') { '&' } else { '?' };
                (format!("{url}{sep}mode=rwc"), sqlite_file_options())
            }
            Backend::Postgres => (url.to_owned(), AnyPoolOptions::new().max_connections(16)),
        };
        let pool = opts.connect(&url).await?;
        Ok(Self { pool, backend })
    }

    pub async fn migrate(&self) -> Result<(), Error> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// Execute a statement that returns no rows.
    pub async fn execute_raw(&self, sql: &str) -> Result<(), Error> {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }
}

fn sqlite_file_options() -> AnyPoolOptions {
    AnyPoolOptions::new()
        .max_connections(8)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::raw_sql(
                    "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
                )
                .execute(conn)
                .await?;
                Ok(())
            })
        })
}

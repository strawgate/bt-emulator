mod common;

use bt_emulator::db::{Backend, Db};

#[tokio::test]
async fn migrations_apply_on_every_backend() {
    for (name, db) in common::backends().await {
        assert_eq!(db.backend().as_str(), name);
        // Migrations are idempotent.
        db.migrate().await.unwrap();
        db.execute_raw("SELECT id FROM projects").await.unwrap();
    }
}

#[tokio::test]
async fn backend_is_detected_from_url() {
    assert_eq!(
        Backend::from_url("sqlite::memory:").unwrap(),
        Backend::Sqlite
    );
    assert_eq!(
        Backend::from_url("sqlite://bt.db").unwrap(),
        Backend::Sqlite
    );
    assert_eq!(
        Backend::from_url("postgres://x/y").unwrap(),
        Backend::Postgres
    );
    assert_eq!(
        Backend::from_url("postgresql://x/y").unwrap(),
        Backend::Postgres
    );
    let err = Backend::from_url("mysql://x/y").unwrap_err();
    assert!(
        err.to_string().contains("unsupported database url"),
        "{err}"
    );
}

#[tokio::test]
async fn connect_rejects_unsupported_url() {
    assert!(Db::connect("mysql://x/y").await.is_err());
}

#[tokio::test]
async fn connect_reports_unreachable_database() {
    let err = Db::connect("sqlite:///nonexistent-dir/x/y.db")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("database error"), "{err}");
}

#[tokio::test]
async fn sqlite_file_database_is_created() {
    let dir = std::env::temp_dir().join(format!("bt-emu-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bt.db");
    Db::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    assert!(path.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

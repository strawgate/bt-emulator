#![allow(dead_code)]

use bt_emulator::db::Db;

/// Every backend the test suite should exercise.
///
/// SQLite (in-memory) always runs. Postgres runs when `BT_TEST_POSTGRES_URL`
/// points at a server we may create databases on; each call gets a fresh
/// database so tests stay isolated.
pub async fn backends() -> Vec<(&'static str, Db)> {
    let mut out = vec![("sqlite", Db::connect("sqlite::memory:").await.unwrap())];
    if let Ok(admin_url) = std::env::var("BT_TEST_POSTGRES_URL") {
        out.push(("postgres", fresh_postgres(&admin_url).await));
    } else if std::env::var_os("BT_REQUIRE_POSTGRES").is_some() {
        panic!("BT_REQUIRE_POSTGRES is set but BT_TEST_POSTGRES_URL is not");
    }
    out
}

async fn fresh_postgres(admin_url: &str) -> Db {
    let admin = Db::connect_without_migrations(admin_url).await.unwrap();
    let name = format!("bt_test_{}", uuid::Uuid::new_v4().simple());
    admin
        .execute_raw(&format!("CREATE DATABASE {name}"))
        .await
        .unwrap();
    let base = admin_url.rsplit_once('/').unwrap().0;
    Db::connect(&format!("{base}/{name}")).await.unwrap()
}

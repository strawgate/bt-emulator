-- Portable DDL: must run unchanged on SQLite and PostgreSQL.
-- Integers are BIGINT (decoded as i64 on both), JSON is stored as TEXT.
CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    org_id TEXT NOT NULL,
    name TEXT NOT NULL,
    created TEXT NOT NULL,
    deleted_at TEXT,
    user_id TEXT,
    settings TEXT
);

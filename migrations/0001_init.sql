-- Portable DDL: must run unchanged on SQLite and PostgreSQL.
-- Integers are BIGINT (decoded as i64 on both); JSON documents are TEXT.

-- Monotonic allocators (transaction ids, object ordering).
CREATE TABLE counters (
    name TEXT PRIMARY KEY,
    value BIGINT NOT NULL
);
INSERT INTO counters (name, value) VALUES ('xact', 0);
INSERT INTO counters (name, value) VALUES ('object', 0);

-- Projects, experiments and datasets. `parent_id` is the org id for projects and
-- the project id otherwise; `data` is the full API object.
CREATE TABLE objects (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    parent_id TEXT NOT NULL,
    name TEXT NOT NULL,
    seq BIGINT NOT NULL,
    deleted BIGINT NOT NULL DEFAULT 0,
    data TEXT NOT NULL
);
CREATE INDEX objects_by_kind ON objects (kind, seq);
CREATE UNIQUE INDEX objects_live_name ON objects (kind, parent_id, name) WHERE deleted = 0;

-- Append-only row versions for experiments, datasets and project logs. The current
-- state of a row is its version with the highest xact_id; `deleted` marks a tombstone.
-- `seq` orders rows written within the same transaction.
CREATE TABLE events (
    object_type TEXT NOT NULL,
    object_id TEXT NOT NULL,
    row_id TEXT NOT NULL,
    xact_id BIGINT NOT NULL,
    seq BIGINT NOT NULL,
    deleted BIGINT NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (object_type, object_id, row_id, xact_id)
);
CREATE INDEX events_by_xact ON events (object_type, object_id, xact_id, seq);

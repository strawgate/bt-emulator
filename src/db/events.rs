//! Row versions for experiments, datasets, project logs and functions.

use serde_json::{Map, Value};
use sqlx::{AnyConnection, Row};

use super::Db;
use crate::error::Error;

/// A row's position in fetch order (newest first): `(xact_id, seq)`.
pub type Position = (i64, i64);

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub xact_id: i64,
    pub seq: i64,
    pub data: Map<String, Value>,
}

impl Event {
    /// A field of the row, `null` when absent (indexing the map would panic).
    pub fn field(&self, key: &str) -> &Value {
        self.data.get(key).unwrap_or(&Value::Null)
    }
}

impl Db {
    /// The current (non-deleted) state of a row, as seen inside a transaction.
    pub async fn current_row(
        conn: &mut AnyConnection,
        object_type: &str,
        object_id: &str,
        row_id: &str,
    ) -> Result<Option<Map<String, Value>>, Error> {
        let row = sqlx::query(
            "SELECT deleted, data FROM events WHERE object_type = $1 AND object_id = $2 AND row_id = $3 \
             ORDER BY xact_id DESC LIMIT 1",
        )
        .bind(object_type)
        .bind(object_id)
        .bind(row_id)
        .fetch_optional(conn)
        .await?;
        let Some(row) = row else { return Ok(None) };
        if row.try_get::<i64, _>("deleted")? != 0 {
            return Ok(None);
        }
        let data: String = row.try_get("data")?;
        Ok(serde_json::from_str(&data)?)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn put_row(
        conn: &mut AnyConnection,
        object_type: &str,
        object_id: &str,
        row_id: &str,
        xact_id: i64,
        seq: i64,
        data: Option<&Map<String, Value>>,
    ) -> Result<(), Error> {
        let json = data.map_or_else(|| "{}".to_owned(), |d| Value::Object(d.clone()).to_string());
        sqlx::query(
            "INSERT INTO events (object_type, object_id, row_id, xact_id, seq, deleted, data) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (object_type, object_id, row_id, xact_id) \
             DO UPDATE SET seq = excluded.seq, deleted = excluded.deleted, data = excluded.data",
        )
        .bind(object_type)
        .bind(object_id)
        .bind(row_id)
        .bind(xact_id)
        .bind(seq)
        .bind(i64::from(data.is_none()))
        .bind(json)
        .execute(conn)
        .await?;
        Ok(())
    }

    /// One page of current rows, newest first, strictly after `after`, as of `version`.
    /// An empty `object_id` reads every object of the type.
    pub async fn rows_page(
        &self,
        object_type: &str,
        object_id: &str,
        version: Option<i64>,
        after: Option<Position>,
        limit: usize,
    ) -> Result<Vec<Event>, Error> {
        let (after_xact, after_seq) = after.unwrap_or((i64::MAX, i64::MAX));
        let rows = sqlx::query(
            "SELECT e.xact_id, e.seq, e.data FROM events e \
             WHERE e.object_type = $1 AND ($2 = '' OR e.object_id = $2) AND e.deleted = 0 AND e.xact_id <= $3 \
               AND (e.xact_id < $4 OR (e.xact_id = $4 AND e.seq < $5)) \
               AND NOT EXISTS (SELECT 1 FROM events w WHERE w.object_type = e.object_type \
                   AND w.object_id = e.object_id AND w.row_id = e.row_id \
                   AND w.xact_id > e.xact_id AND w.xact_id <= $3) \
             ORDER BY e.xact_id DESC, e.seq DESC LIMIT $6",
        )
        .bind(object_type)
        .bind(object_id)
        .bind(version.unwrap_or(i64::MAX))
        .bind(after_xact)
        .bind(after_seq)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|r| {
                let data: String = r.try_get("data")?;
                Ok(Event {
                    xact_id: r.try_get("xact_id")?,
                    seq: r.try_get("seq")?,
                    data: serde_json::from_str(&data)?,
                })
            })
            .collect()
    }

    /// A row by id within any object of the type, as of `version`, with its object id.
    /// `None` when it never existed or its latest version is a deletion.
    pub async fn row_at(
        &self,
        object_type: &str,
        row_id: &str,
        version: Option<i64>,
    ) -> Result<Option<(String, Event)>, Error> {
        let row = sqlx::query(
            "SELECT object_id, xact_id, seq, deleted, data FROM events \
             WHERE object_type = $1 AND row_id = $2 AND xact_id <= $3 \
             ORDER BY xact_id DESC LIMIT 1",
        )
        .bind(object_type)
        .bind(row_id)
        .bind(version.unwrap_or(i64::MAX))
        .fetch_optional(self.pool())
        .await?;
        let Some(row) = row else { return Ok(None) };
        if row.try_get::<i64, _>("deleted")? != 0 {
            return Ok(None);
        }
        let data: String = row.try_get("data")?;
        let event = Event {
            xact_id: row.try_get("xact_id")?,
            seq: row.try_get("seq")?,
            data: serde_json::from_str(&data)?,
        };
        Ok(Some((row.try_get("object_id")?, event)))
    }

    /// Every current row of an object, newest first.
    pub async fn all_rows(&self, object_type: &str, object_id: &str) -> Result<Vec<Event>, Error> {
        self.rows_page(object_type, object_id, None, None, usize::MAX)
            .await
    }
}

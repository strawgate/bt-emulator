//! Projects, experiments and datasets, and the other named `/v1` resources.

use serde_json::Value;
use sqlx::Row;
use sqlx::any::AnyRow;

use super::Db;
use crate::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Project,
    Experiment,
    Dataset,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Experiment => "experiment",
            Self::Dataset => "dataset",
        }
    }
}

/// A stored object: its API representation plus its creation order.
#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    pub seq: i64,
    pub data: Value,
}

impl Object {
    pub fn id(&self) -> &str {
        self.data["id"].as_str().unwrap_or_default()
    }
    pub fn name(&self) -> &str {
        self.data["name"].as_str().unwrap_or_default()
    }
}

fn decode(row: &AnyRow) -> Result<Object, Error> {
    let data: String = row.try_get("data")?;
    Ok(Object {
        seq: row.try_get("seq")?,
        data: serde_json::from_str(&data)?,
    })
}

/// The field that names an object uniquely within its parent: an environment's
/// `slug`, everything else's `name`.
pub fn identity(kind: &str) -> &'static str {
    if kind == "environment" {
        "slug"
    } else {
        "name"
    }
}

impl Db {
    /// All live objects of a kind, newest first.
    pub async fn objects(&self, kind: Kind) -> Result<Vec<Object>, Error> {
        self.objects_of(kind.as_str()).await
    }

    /// All live objects of a kind named by string, newest first.
    pub async fn objects_of(&self, kind: &str) -> Result<Vec<Object>, Error> {
        sqlx::query(
            "SELECT seq, data FROM objects WHERE kind = $1 AND deleted = 0 ORDER BY seq DESC",
        )
        .bind(kind)
        .fetch_all(self.pool())
        .await?
        .iter()
        .map(decode)
        .collect()
    }

    pub async fn object(&self, kind: Kind, id: &str) -> Result<Option<Object>, Error> {
        self.object_of(kind.as_str(), id).await
    }

    pub async fn object_of(&self, kind: &str, id: &str) -> Result<Option<Object>, Error> {
        let row = sqlx::query(
            "SELECT seq, data FROM objects WHERE kind = $1 AND id = $2 AND deleted = 0",
        )
        .bind(kind)
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(decode).transpose()
    }

    pub async fn object_by_name(
        &self,
        kind: Kind,
        parent_id: &str,
        name: &str,
    ) -> Result<Option<Object>, Error> {
        self.object_by_name_of(kind.as_str(), parent_id, name).await
    }

    pub async fn object_by_name_of(
        &self,
        kind: &str,
        parent_id: &str,
        name: &str,
    ) -> Result<Option<Object>, Error> {
        let row = sqlx::query(
            "SELECT seq, data FROM objects WHERE kind = $1 AND parent_id = $2 AND name = $3 AND deleted = 0",
        )
        .bind(kind)
        .bind(parent_id)
        .bind(name)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(decode).transpose()
    }

    /// Insert a new object. Callers hold the write lock and have checked name uniqueness.
    pub async fn insert_object(
        &self,
        kind: Kind,
        parent_id: &str,
        data: Value,
    ) -> Result<Object, Error> {
        self.insert_object_of(kind.as_str(), parent_id, data).await
    }

    pub async fn insert_object_of(
        &self,
        kind: &str,
        parent_id: &str,
        data: Value,
    ) -> Result<Object, Error> {
        let seq = self.next_counter("object").await?;
        let id = data["id"].as_str().unwrap_or_default();
        let name = data[identity(kind)].as_str().unwrap_or_default();
        sqlx::query("INSERT INTO objects (id, kind, parent_id, name, seq, deleted, data) VALUES ($1, $2, $3, $4, $5, 0, $6)")
            .bind(id)
            .bind(kind)
            .bind(parent_id)
            .bind(name)
            .bind(seq)
            .bind(data.to_string())
            .execute(self.pool())
            .await?;
        Ok(Object { seq, data })
    }

    /// Replace an object's data (and name). `deleted` soft-deletes it.
    pub async fn update_object(&self, data: &Value, deleted: bool) -> Result<(), Error> {
        sqlx::query(
            "UPDATE objects SET name = CASE WHEN kind = 'environment' THEN $5 ELSE $1 END, \
             deleted = $2, data = $3 WHERE id = $4",
        )
        .bind(data["name"].as_str().unwrap_or_default())
        .bind(i64::from(deleted))
        .bind(data.to_string())
        .bind(data["id"].as_str().unwrap_or_default())
        .bind(data["slug"].as_str().unwrap_or_default())
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

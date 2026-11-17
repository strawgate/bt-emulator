//! Attachment metadata and content.

use serde_json::Value;
use sqlx::Row;

use super::Db;
use crate::error::Error;

#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    pub key: String,
    pub filename: String,
    pub content_type: String,
    pub token: String,
    pub status: Value,
    /// Base64 content, once uploaded.
    pub data: Option<String>,
}

impl Db {
    /// Record an attachment's metadata, keeping its token, status and content if
    /// the key is already known.
    pub async fn put_attachment(&self, a: &Attachment) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO attachments (key, filename, content_type, token, status, data) VALUES ($1, $2, $3, $4, $5, NULL) \
             ON CONFLICT (key) DO UPDATE SET filename = excluded.filename, content_type = excluded.content_type",
        )
        .bind(&a.key)
        .bind(&a.filename)
        .bind(&a.content_type)
        .bind(&a.token)
        .bind(a.status.to_string())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn attachment(&self, key: &str) -> Result<Option<Attachment>, Error> {
        let row = sqlx::query(
            "SELECT key, filename, content_type, token, status, data FROM attachments WHERE key = $1",
        )
        .bind(key)
        .fetch_optional(self.pool())
        .await?;
        let Some(row) = row else { return Ok(None) };
        let status: String = row.try_get("status")?;
        Ok(Some(Attachment {
            key: row.try_get("key")?,
            filename: row.try_get("filename")?,
            content_type: row.try_get("content_type")?,
            token: row.try_get("token")?,
            status: serde_json::from_str(&status)?,
            data: row.try_get("data")?,
        }))
    }

    pub async fn set_attachment_data(&self, key: &str, data: &str) -> Result<(), Error> {
        sqlx::query("UPDATE attachments SET data = $1 WHERE key = $2")
            .bind(data)
            .bind(key)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn set_attachment_status(&self, key: &str, status: &Value) -> Result<(), Error> {
        sqlx::query("UPDATE attachments SET status = $1 WHERE key = $2")
            .bind(status.to_string())
            .bind(key)
            .execute(self.pool())
            .await?;
        Ok(())
    }
}

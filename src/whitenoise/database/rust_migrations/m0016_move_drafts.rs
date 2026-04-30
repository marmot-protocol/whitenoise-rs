use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        16
    }

    fn description(&self) -> &'static str {
        "Move drafts into per-account database"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        // Per-account schema: no account_pubkey column. The FK to
        // group_information is dropped because group_information lives in the
        // shared DB — application-level checks replace it.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS drafts (
                id                 INTEGER PRIMARY KEY AUTOINCREMENT,
                mls_group_id       BLOB NOT NULL UNIQUE,
                content            TEXT NOT NULL DEFAULT '',
                reply_to_id        TEXT
                    CHECK (reply_to_id IS NULL OR (length(reply_to_id) = 64 AND reply_to_id NOT GLOB '*[^0-9a-fA-F]*')),
                media_attachments  JSONB NOT NULL DEFAULT '[]',
                created_at         INTEGER NOT NULL,
                updated_at         INTEGER NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await?;

        let shared_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='drafts')",
        )
        .fetch_one(global_db)
        .await?;

        if !shared_table_exists {
            return Ok(());
        }

        let rows: Vec<(Vec<u8>, String, Option<String>, String, i64, i64)> = sqlx::query_as(
            "SELECT mls_group_id, content, reply_to_id, media_attachments, created_at, updated_at \
             FROM drafts WHERE account_pubkey = ?",
        )
        .bind(account_pubkey)
        .fetch_all(global_db)
        .await?;

        for (mls_group_id, content, reply_to_id, media_attachments, created_at, updated_at) in rows
        {
            sqlx::query(
                "INSERT OR IGNORE INTO drafts \
                    (mls_group_id, content, reply_to_id, media_attachments, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(mls_group_id)
            .bind(content)
            .bind(reply_to_id)
            .bind(media_attachments)
            .bind(created_at)
            .bind(updated_at)
            .execute(&mut *tx)
            .await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sqlx::SqlitePool;
    use tempfile::TempDir;

    use super::*;
    use crate::whitenoise::database::rust_migrations::Migrator;

    async fn pools() -> (SqlitePool, SqlitePool, TempDir) {
        let dir = TempDir::new().unwrap();
        let local = SqlitePool::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("local.db").display()
        ))
        .await
        .unwrap();
        let global = SqlitePool::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("global.db").display()
        ))
        .await
        .unwrap();
        (local, global, dir)
    }

    #[tokio::test]
    async fn migration_copies_account_drafts_only() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        let other_pubkey = "cd".repeat(32);

        sqlx::query(
            "CREATE TABLE drafts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey TEXT NOT NULL,
                mls_group_id BLOB NOT NULL,
                content TEXT NOT NULL,
                reply_to_id TEXT,
                media_attachments JSONB NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE(account_pubkey, mls_group_id)
            )",
        )
        .execute(&global)
        .await
        .unwrap();

        let now = Utc::now().timestamp_millis();
        for (acc, group, content) in &[
            (&pubkey, b"group_one_______", "mine"),
            (&pubkey, b"group_two_______", "also mine"),
            (&other_pubkey, b"group_one_______", "not mine"),
        ] {
            sqlx::query(
                "INSERT INTO drafts \
                    (account_pubkey, mls_group_id, content, reply_to_id, \
                     media_attachments, created_at, updated_at) \
                 VALUES (?, ?, ?, NULL, '[]', ?, ?)",
            )
            .bind(acc)
            .bind(group.as_slice())
            .bind(content)
            .bind(now)
            .bind(now)
            .execute(&global)
            .await
            .unwrap();
        }

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let contents: Vec<(String,)> =
            sqlx::query_as("SELECT content FROM drafts ORDER BY content")
                .fetch_all(&local)
                .await
                .unwrap();
        let actual: Vec<&str> = contents.iter().map(|(c,)| c.as_str()).collect();
        assert_eq!(actual, vec!["also mine", "mine"]);
    }

    #[tokio::test]
    async fn migration_tolerates_missing_shared_table() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ef".repeat(32);

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='drafts')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);
    }
}

use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        19
    }

    fn description(&self) -> &'static str {
        "Move account-scoped processed_events into per-account database"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        // Per-account schema: drops the `account_pubkey` column and the
        // `idx_processed_events_global_unique` partial index, since this file
        // only stores account-scoped rows. Author column stays nullable for
        // any future per-account author-keyed lookups.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS processed_events (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id            TEXT NOT NULL UNIQUE
                    CHECK (length(event_id) = 64 AND event_id NOT GLOB '*[^0-9a-fA-F]*'),
                created_at          DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                event_created_at    INTEGER DEFAULT NULL,
                event_kind          INTEGER DEFAULT NULL,
                author              TEXT DEFAULT NULL
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_processed_events_kind_timestamp
                ON processed_events(event_kind, event_created_at)",
        )
        .execute(&mut *tx)
        .await?;

        let shared_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='processed_events')",
        )
        .fetch_one(global_db)
        .await?;

        if !shared_table_exists {
            return Ok(());
        }

        type Row = (String, String, Option<i64>, Option<i64>, Option<String>);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT event_id, created_at, event_created_at, event_kind, author \
             FROM processed_events WHERE account_pubkey = ?",
        )
        .bind(account_pubkey)
        .fetch_all(global_db)
        .await?;

        for (event_id, created_at, event_created_at, event_kind, author) in rows {
            sqlx::query(
                "INSERT OR IGNORE INTO processed_events \
                    (event_id, created_at, event_created_at, event_kind, author) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(event_id)
            .bind(created_at)
            .bind(event_created_at)
            .bind(event_kind)
            .bind(author)
            .execute(&mut *tx)
            .await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
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

    async fn seed_global(global: &SqlitePool) {
        sqlx::query(
            "CREATE TABLE processed_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL
                    CHECK (length(event_id) = 64 AND event_id NOT GLOB '*[^0-9a-fA-F]*'),
                account_pubkey TEXT,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                event_created_at INTEGER DEFAULT NULL,
                event_kind INTEGER DEFAULT NULL,
                author TEXT DEFAULT NULL,
                UNIQUE(event_id, account_pubkey)
            )",
        )
        .execute(global)
        .await
        .unwrap();
        sqlx::query(
            "CREATE UNIQUE INDEX idx_processed_events_global_unique
                ON processed_events(event_id) WHERE account_pubkey IS NULL",
        )
        .execute(global)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn migration_copies_only_this_accounts_rows() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        let other_pubkey = "cd".repeat(32);
        seed_global(&global).await;

        // Account-scoped row for our account.
        sqlx::query(
            "INSERT INTO processed_events (event_id, account_pubkey, event_kind) VALUES (?, ?, 3)",
        )
        .bind("11".repeat(32))
        .bind(&pubkey)
        .execute(&global)
        .await
        .unwrap();
        // Account-scoped row for a different account.
        sqlx::query(
            "INSERT INTO processed_events (event_id, account_pubkey, event_kind) VALUES (?, ?, 3)",
        )
        .bind("22".repeat(32))
        .bind(&other_pubkey)
        .execute(&global)
        .await
        .unwrap();
        // Global row (NULL account) — must NOT be copied.
        sqlx::query(
            "INSERT INTO processed_events (event_id, account_pubkey, event_kind) VALUES (?, NULL, 0)",
        )
        .bind("33".repeat(32))
        .execute(&global)
        .await
        .unwrap();

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let event_ids: Vec<(String,)> =
            sqlx::query_as("SELECT event_id FROM processed_events ORDER BY event_id")
                .fetch_all(&local)
                .await
                .unwrap();
        assert_eq!(event_ids.len(), 1);
        assert_eq!(event_ids[0].0, "11".repeat(32));
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
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='processed_events')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);
    }
}

use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        18
    }

    fn description(&self) -> &'static str {
        "Move published_events into per-account database"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        // Per-account schema: no account_pubkey column (the file is the scope).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS published_events (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id    TEXT NOT NULL UNIQUE
                    CHECK (length(event_id) = 64 AND event_id GLOB '[0-9a-fA-F]*'),
                created_at  DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_published_events_event_id ON published_events(event_id)")
            .execute(&mut *tx)
            .await?;

        // Copy this account's rows from shared DB, if any. Tolerant of missing
        // shared.published_events (fresh install, or table already dropped by
        // the post-local cleanup migration).
        let shared_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='published_events')",
        )
        .fetch_one(global_db)
        .await?;

        if !shared_table_exists {
            return Ok(());
        }

        type Row = (String, String);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT event_id, created_at FROM published_events WHERE account_pubkey = ?",
        )
        .bind(account_pubkey)
        .fetch_all(global_db)
        .await?;

        for (event_id, created_at) in rows {
            sqlx::query(
                "INSERT OR IGNORE INTO published_events (event_id, created_at) VALUES (?, ?)",
            )
            .bind(event_id)
            .bind(created_at)
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
            "CREATE TABLE published_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL
                    CHECK (length(event_id) = 64 AND event_id GLOB '[0-9a-fA-F]*'),
                account_pubkey TEXT NOT NULL,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(event_id, account_pubkey)
            )",
        )
        .execute(global)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn migration_copies_account_rows_only() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        let other_pubkey = "cd".repeat(32);
        seed_global(&global).await;

        for (acc, evt) in &[
            (&pubkey, "11".repeat(32)),
            (&pubkey, "22".repeat(32)),
            (&other_pubkey, "33".repeat(32)),
        ] {
            sqlx::query("INSERT INTO published_events (event_id, account_pubkey) VALUES (?, ?)")
                .bind(evt)
                .bind(acc)
                .execute(&global)
                .await
                .unwrap();
        }

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let event_ids: Vec<(String,)> =
            sqlx::query_as("SELECT event_id FROM published_events ORDER BY event_id")
                .fetch_all(&local)
                .await
                .unwrap();
        assert_eq!(
            event_ids.iter().map(|(e,)| e.as_str()).collect::<Vec<_>>(),
            vec!["11".repeat(32).as_str(), "22".repeat(32).as_str()]
        );
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
             WHERE type='table' AND name='published_events')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);
    }
}

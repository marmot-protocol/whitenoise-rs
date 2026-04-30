use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        15
    }

    fn description(&self) -> &'static str {
        "Move account_settings into per-account database"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        // Per-account schema: no account_pubkey column (the file is the scope),
        // singleton row enforced by `id = 1`.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS account_settings (
                id                    INTEGER PRIMARY KEY CHECK (id = 1),
                notifications_enabled INTEGER NOT NULL DEFAULT 1,
                created_at            INTEGER NOT NULL,
                updated_at            INTEGER NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await?;

        // Copy this account's row from shared DB, if any. Tolerant of missing
        // shared.account_settings (fresh install, or table already dropped by
        // the post-local cleanup migration). If the source table is missing
        // because v21 dropped it before this v15 ran for the current account
        // (a sequencing bug — `MIGRATOR.run_all` at boot is supposed to make
        // this impossible), log a warn so the data loss isn't silent.
        let shared_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='account_settings')",
        )
        .fetch_one(global_db)
        .await?;

        if !shared_table_exists {
            tracing::warn!(
                target: "whitenoise::database::rust_migrations::m0015",
                account = account_pubkey,
                "shared.account_settings is missing; skipping local copy. \
                 Expected only on fresh installs or when v21 already dropped \
                 the table — flag this if it appears for an upgrading account."
            );
        }

        if shared_table_exists {
            let row: Option<(i64, i64, i64)> = sqlx::query_as(
                "SELECT notifications_enabled, created_at, updated_at \
                 FROM account_settings WHERE account_pubkey = ?",
            )
            .bind(account_pubkey)
            .fetch_optional(global_db)
            .await?;

            if let Some((notifications_enabled, created_at, updated_at)) = row {
                sqlx::query(
                    "INSERT OR IGNORE INTO account_settings \
                        (id, notifications_enabled, created_at, updated_at) \
                     VALUES (1, ?, ?, ?)",
                )
                .bind(notifications_enabled)
                .bind(created_at)
                .bind(updated_at)
                .execute(&mut *tx)
                .await?;
            }
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

    /// Seed shared `account_settings` with one row keyed by `account_pubkey`.
    async fn seed_global(global: &SqlitePool, account_pubkey: &str, enabled: bool) {
        sqlx::query(
            "CREATE TABLE account_settings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey TEXT NOT NULL UNIQUE,
                notifications_enabled INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )",
        )
        .execute(global)
        .await
        .unwrap();

        let now = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO account_settings \
                (account_pubkey, notifications_enabled, created_at, updated_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(account_pubkey)
        .bind(if enabled { 1 } else { 0 })
        .bind(now)
        .bind(now)
        .execute(global)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn migration_copies_existing_row_to_local() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        seed_global(&global, &pubkey, false).await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let row: (i64, i64) =
            sqlx::query_as("SELECT id, notifications_enabled FROM account_settings")
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(row.0, 1, "row must be the singleton id=1");
        assert_eq!(row.1, 0, "notifications_enabled should round-trip");
    }

    #[tokio::test]
    async fn migration_creates_table_when_no_existing_row() {
        let (local, global, _dir) = pools().await;
        let pubkey = "cd".repeat(32);

        // Seed an empty shared.account_settings (other accounts may use it).
        sqlx::query(
            "CREATE TABLE account_settings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey TEXT NOT NULL UNIQUE,
                notifications_enabled INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )",
        )
        .execute(&global)
        .await
        .unwrap();

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='account_settings')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists, "local table should exist even with no source row");

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM account_settings")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn migration_tolerates_missing_shared_table() {
        // Post-cleanup state: the shared account_settings table is gone, but a
        // brand-new account that never copied still needs a working local
        // schema. Migration must succeed.
        let (local, global, _dir) = pools().await;
        let pubkey = "ef".repeat(32);

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='account_settings')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);
    }
}

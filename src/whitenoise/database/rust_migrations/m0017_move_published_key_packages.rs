use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        17
    }

    fn description(&self) -> &'static str {
        "Move published_key_packages into per-account database"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS published_key_packages (
                id                    INTEGER PRIMARY KEY AUTOINCREMENT,
                key_package_hash_ref  BLOB NOT NULL,
                event_id              TEXT NOT NULL UNIQUE,
                kind                  INTEGER NOT NULL DEFAULT 443,
                d_tag                 TEXT NULL,
                consumed_at           INTEGER,
                key_material_deleted  INTEGER NOT NULL DEFAULT 0,
                created_at            INTEGER NOT NULL DEFAULT (unixepoch())
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_published_kp_event_id
                ON published_key_packages(event_id)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_published_kp_cleanup
                ON published_key_packages(consumed_at, key_material_deleted)",
        )
        .execute(&mut *tx)
        .await?;

        let shared_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='published_key_packages')",
        )
        .fetch_one(global_db)
        .await?;

        if !shared_table_exists {
            return Ok(());
        }

        type Row = (Vec<u8>, String, i64, Option<String>, Option<i64>, i64, i64);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT key_package_hash_ref, event_id, kind, d_tag, \
                    consumed_at, key_material_deleted, created_at \
             FROM published_key_packages WHERE account_pubkey = ?",
        )
        .bind(account_pubkey)
        .fetch_all(global_db)
        .await?;

        for (hash_ref, event_id, kind, d_tag, consumed_at, key_material_deleted, created_at) in rows
        {
            sqlx::query(
                "INSERT OR IGNORE INTO published_key_packages \
                    (key_package_hash_ref, event_id, kind, d_tag, \
                     consumed_at, key_material_deleted, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(hash_ref)
            .bind(event_id)
            .bind(kind)
            .bind(d_tag)
            .bind(consumed_at)
            .bind(key_material_deleted)
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

    #[tokio::test]
    async fn migration_copies_account_packages_only() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        let other_pubkey = "cd".repeat(32);

        sqlx::query(
            "CREATE TABLE published_key_packages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey TEXT NOT NULL,
                key_package_hash_ref BLOB NOT NULL,
                event_id TEXT NOT NULL,
                kind INTEGER NOT NULL DEFAULT 443,
                d_tag TEXT NULL,
                consumed_at INTEGER,
                key_material_deleted INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                UNIQUE(account_pubkey, event_id)
            )",
        )
        .execute(&global)
        .await
        .unwrap();

        for (acc, evt) in &[
            (&pubkey, "evt-mine-1"),
            (&pubkey, "evt-mine-2"),
            (&other_pubkey, "evt-other"),
        ] {
            sqlx::query(
                "INSERT INTO published_key_packages \
                    (account_pubkey, key_package_hash_ref, event_id) VALUES (?, ?, ?)",
            )
            .bind(acc)
            .bind(b"hash" as &[u8])
            .bind(evt)
            .execute(&global)
            .await
            .unwrap();
        }

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let event_ids: Vec<(String,)> =
            sqlx::query_as("SELECT event_id FROM published_key_packages ORDER BY event_id")
                .fetch_all(&local)
                .await
                .unwrap();
        assert_eq!(
            event_ids.iter().map(|(e,)| e.as_str()).collect::<Vec<_>>(),
            vec!["evt-mine-1", "evt-mine-2"]
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
             WHERE type='table' AND name='published_key_packages')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);
    }
}

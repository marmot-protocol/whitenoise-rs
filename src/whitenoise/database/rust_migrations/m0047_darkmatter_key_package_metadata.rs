use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        47
    }

    fn description(&self) -> &'static str {
        "Add Darkmatter key package metadata"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        _global_db: &SqlitePool,
        _account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        sqlx::query("ALTER TABLE published_key_packages ADD COLUMN key_package_ref BLOB NULL")
            .execute(&mut *tx)
            .await?;

        sqlx::query(
            "ALTER TABLE published_key_packages
             ADD COLUMN app_components TEXT NOT NULL DEFAULT '[]'",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "ALTER TABLE published_key_packages
             ADD COLUMN package_version INTEGER NOT NULL DEFAULT 1",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "ALTER TABLE published_key_packages
             ADD COLUMN package_role TEXT NOT NULL DEFAULT 'legacy'
             CHECK (package_role IN ('legacy', 'last_resort', 'rotated'))",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX idx_published_kp_key_package_ref
                ON published_key_packages(key_package_ref)",
        )
        .execute(&mut *tx)
        .await?;

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
    async fn adds_darkmatter_metadata_columns_with_legacy_defaults() {
        let (local, global, _dir) = pools().await;
        create_legacy_table(&local).await;

        sqlx::query(
            "INSERT INTO published_key_packages (key_package_hash_ref, event_id, kind, d_tag)
             VALUES (?, 'legacy-event', 30443, 'legacy-slot')",
        )
        .bind(&[1_u8, 2, 3] as &[u8])
        .execute(&local)
        .await
        .unwrap();

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &"aa".repeat(32))))
            .await
            .unwrap();

        let row: (Option<Vec<u8>>, String, i64, String) = sqlx::query_as(
            "SELECT key_package_ref, app_components, package_version, package_role
             FROM published_key_packages
             WHERE event_id = 'legacy-event'",
        )
        .fetch_one(&local)
        .await
        .unwrap();

        assert_eq!(row, (None, "[]".to_string(), 1, "legacy".to_string()));

        let index_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master
             WHERE type='index' AND name='idx_published_kp_key_package_ref')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(index_exists);
    }

    async fn create_legacy_table(pool: &SqlitePool) {
        sqlx::query(
            "CREATE TABLE published_key_packages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                key_package_hash_ref BLOB NOT NULL,
                event_id TEXT NOT NULL UNIQUE,
                kind INTEGER NOT NULL DEFAULT 30443,
                d_tag TEXT NULL,
                consumed_at INTEGER,
                key_material_deleted INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (unixepoch())
            )",
        )
        .execute(pool)
        .await
        .unwrap();
    }
}

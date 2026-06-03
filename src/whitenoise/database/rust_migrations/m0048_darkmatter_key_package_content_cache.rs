use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        48
    }

    fn description(&self) -> &'static str {
        "Cache Darkmatter key package content"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        _global_db: &SqlitePool,
        _account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        sqlx::query("ALTER TABLE published_key_packages ADD COLUMN key_package_content TEXT NULL")
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
    use crate::whitenoise::database::rust_migrations::m0047_darkmatter_key_package_metadata;

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
    async fn adds_nullable_key_package_content_cache_column() {
        let (local, global, _dir) = pools().await;
        create_legacy_table(&local).await;

        Migrator::new(
            vec![],
            vec![
                Box::new(m0047_darkmatter_key_package_metadata::Migration),
                Box::new(Migration),
            ],
        )
        .run(&global, Some((&local, &"aa".repeat(32))))
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO published_key_packages
             (key_package_hash_ref, key_package_ref, key_package_content, event_id, kind, d_tag,
              app_components, package_version, package_role)
             VALUES (?, ?, 'base64-key-package', 'darkmatter-event', 30443, 'stable-slot',
                     '[\"0x8001\"]', 2, 'last_resort')",
        )
        .bind(&[1_u8, 2, 3] as &[u8])
        .bind(&[4_u8, 5, 6] as &[u8])
        .execute(&local)
        .await
        .unwrap();

        let content: Option<String> = sqlx::query_scalar(
            "SELECT key_package_content FROM published_key_packages
             WHERE event_id = 'darkmatter-event'",
        )
        .fetch_one(&local)
        .await
        .unwrap();

        assert_eq!(content.as_deref(), Some("base64-key-package"));
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

use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        49
    }

    fn description(&self) -> &'static str {
        "Create Marmot message projections"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        _global_db: &SqlitePool,
        _account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS marmot_message_projections (
                message_id TEXT NOT NULL
                    CHECK (length(message_id) = 64 AND message_id GLOB '[0-9a-fA-F]*'),
                mls_group_id BLOB NOT NULL,
                created_at INTEGER NOT NULL,
                processed_at INTEGER NOT NULL,
                message_json TEXT NOT NULL,
                PRIMARY KEY (message_id, mls_group_id)
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_marmot_message_projections_group
                ON marmot_message_projections(mls_group_id, created_at DESC, processed_at DESC)",
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
    async fn creates_marmot_message_projection_table() {
        let (local, global, _dir) = pools().await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &"aa".repeat(32))))
            .await
            .unwrap();

        let group_id = vec![0xAB; 32];
        sqlx::query(
            "INSERT INTO marmot_message_projections
             (message_id, mls_group_id, created_at, processed_at, message_json)
             VALUES (?, ?, 1, 2, '{}')",
        )
        .bind("11".repeat(32))
        .bind(&group_id)
        .execute(&local)
        .await
        .unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM marmot_message_projections")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}

use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        46
    }

    fn description(&self) -> &'static str {
        "Create account maintenance task markers"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        _global_db: &SqlitePool,
        _account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS account_maintenance_tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                completed_at INTEGER NOT NULL
            )",
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
    async fn creates_account_maintenance_tasks_table() {
        let (local, global, _dir) = pools().await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &"aa".repeat(32))))
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='account_maintenance_tasks')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);
    }

    #[tokio::test]
    async fn migration_is_idempotent() {
        let (local, global, _dir) = pools().await;
        let migrator = Migrator::new(vec![], vec![Box::new(Migration)]);
        let account = "bb".repeat(32);

        migrator
            .run(&global, Some((&local, &account)))
            .await
            .unwrap();
        migrator
            .run(&global, Some((&local, &account)))
            .await
            .unwrap();

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM _rust_migrations WHERE version = 46")
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(count, 1);
    }
}

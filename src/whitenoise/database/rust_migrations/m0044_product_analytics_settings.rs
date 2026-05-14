use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        44
    }

    fn description(&self) -> &'static str {
        "Create product analytics consent settings"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS product_analytics_settings (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                enabled INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
                consent_version TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
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

    use super::*;
    use crate::whitenoise::database::rust_migrations::Migrator;

    #[tokio::test]
    async fn creates_product_analytics_settings_table() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();

        Migrator::new(vec![Box::new(Migration)], vec![])
            .run(&pool, None)
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='product_analytics_settings')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(exists);
    }

    #[tokio::test]
    async fn idempotent_when_table_exists() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();

        Migrator::new(vec![Box::new(Migration)], vec![])
            .run(&pool, None)
            .await
            .unwrap();
        Migrator::new(vec![Box::new(Migration)], vec![])
            .run(&pool, None)
            .await
            .unwrap();
    }
}

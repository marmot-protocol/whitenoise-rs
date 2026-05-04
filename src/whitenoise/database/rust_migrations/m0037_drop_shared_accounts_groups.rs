use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        37
    }

    fn description(&self) -> &'static str {
        "Drop shared accounts_groups table (data moved to per-account DBs in v36)"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        sqlx::query("DROP TABLE IF EXISTS accounts_groups")
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
    async fn drops_table_when_present() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE accounts_groups (
                id INTEGER PRIMARY KEY,
                account_pubkey TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        Migrator::new(vec![Box::new(Migration)], vec![])
            .run(&pool, None)
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='accounts_groups')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!exists);
    }

    #[tokio::test]
    async fn idempotent_when_table_absent() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        Migrator::new(vec![Box::new(Migration)], vec![])
            .run(&pool, None)
            .await
            .unwrap();
    }
}

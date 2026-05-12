use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        41
    }

    fn description(&self) -> &'static str {
        "Drop shared aggregated_messages table (data moved to per-account DBs in v38)"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        sqlx::query("DROP TABLE IF EXISTS aggregated_messages")
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
            "CREATE TABLE aggregated_messages (
                id INTEGER PRIMARY KEY,
                message_id TEXT NOT NULL,
                mls_group_id BLOB NOT NULL
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
             WHERE type='table' AND name='aggregated_messages')",
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

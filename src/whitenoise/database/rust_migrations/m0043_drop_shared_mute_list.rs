use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        43
    }

    fn description(&self) -> &'static str {
        "Drop shared mute_list table (data moved to per-account DBs in v42)"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        sqlx::query("DROP INDEX IF EXISTS idx_mute_list_account")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DROP TABLE IF EXISTS mute_list")
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
            "CREATE TABLE mute_list (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey TEXT NOT NULL,
                muted_pubkey   TEXT NOT NULL,
                is_private     INTEGER NOT NULL DEFAULT 1,
                created_at     INTEGER NOT NULL,
                UNIQUE(account_pubkey, muted_pubkey)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("CREATE INDEX idx_mute_list_account ON mute_list(account_pubkey)")
            .execute(&pool)
            .await
            .unwrap();

        Migrator::new(vec![Box::new(Migration)], vec![])
            .run(&pool, None)
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='mute_list')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!exists);

        let index_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='index' AND name='idx_mute_list_account')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!index_exists);
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

use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        28
    }

    fn description(&self) -> &'static str {
        "Purge account-scoped rows from shared processed_events (moved at v21)"
    }

    /// `processed_events` keeps NULL-scoped rows in the shared DB; only the
    /// account-scoped rows have been moved out by v21. This deletion runs
    /// after every account on disk has applied v21 locally — the unified
    /// timeline guarantees v21 < v28 ordering, and `MIGRATOR.run_all` at
    /// boot (driven by `Whitenoise::enumerate_account_pools`) walks every
    /// per-account file in lockstep with shared so v28 cannot fire ahead
    /// of v21 for any account, including ones that have never logged in.
    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        let table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='processed_events')",
        )
        .fetch_one(&mut *tx)
        .await?;

        if !table_exists {
            return Ok(());
        }

        sqlx::query("DELETE FROM processed_events WHERE account_pubkey IS NOT NULL")
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
    async fn purges_account_rows_keeps_global_rows() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE processed_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL,
                account_pubkey TEXT
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO processed_events (event_id, account_pubkey) VALUES (?, ?)")
            .bind("11".repeat(32))
            .bind("ab".repeat(32))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO processed_events (event_id, account_pubkey) VALUES (?, NULL)")
            .bind("22".repeat(32))
            .execute(&pool)
            .await
            .unwrap();

        Migrator::new(vec![Box::new(Migration)], vec![])
            .run(&pool, None)
            .await
            .unwrap();

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM processed_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "global row should be retained");

        let (account_pubkey,): (Option<String>,) =
            sqlx::query_as("SELECT account_pubkey FROM processed_events")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(account_pubkey.is_none());
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

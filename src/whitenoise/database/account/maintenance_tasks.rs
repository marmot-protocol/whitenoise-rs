//! Per-account repository for one-time maintenance task markers.

use std::sync::Arc;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::account_db::AccountDatabase;
use crate::whitenoise::error::Result;

/// Repository for account-scoped maintenance task completion markers.
#[derive(Clone, Debug)]
pub struct MaintenanceTasksRepo {
    db: Arc<AccountDatabase>,
}

impl MaintenanceTasksRepo {
    pub(crate) fn new(db: Arc<AccountDatabase>) -> Self {
        Self { db }
    }

    /// Returns true once `name` has completed successfully for this account.
    pub async fn is_completed(&self, name: &str) -> Result<bool> {
        let completed: i64 = sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM account_maintenance_tasks WHERE name = ?
             )",
        )
        .bind(name)
        .fetch_one(&self.db.inner.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(completed != 0)
    }

    /// Mark `name` complete for this account.
    ///
    /// This is intentionally an upsert so retrying the same successful
    /// maintenance task remains idempotent.
    pub async fn mark_completed(&self, name: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO account_maintenance_tasks (name, completed_at)
             VALUES (?, unixepoch())
             ON CONFLICT(name) DO UPDATE SET completed_at = excluded.completed_at",
        )
        .bind(name)
        .execute(&self.db.inner.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nostr_sdk::Keys;
    use tempfile::TempDir;

    use super::*;

    async fn setup() -> (MaintenanceTasksRepo, TempDir) {
        let dir = TempDir::new().unwrap();
        let pubkey = Keys::generate().public_key();
        let db = Arc::new(
            AccountDatabase::new(pubkey, dir.path().join("acct.db"))
                .await
                .unwrap(),
        );

        sqlx::query("DROP TABLE IF EXISTS account_maintenance_tasks")
            .execute(&db.inner.pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE account_maintenance_tasks (
                name TEXT PRIMARY KEY,
                completed_at INTEGER NOT NULL
            )",
        )
        .execute(&db.inner.pool)
        .await
        .unwrap();

        (MaintenanceTasksRepo::new(db), dir)
    }

    #[tokio::test]
    async fn task_is_incomplete_until_marked() {
        let (repo, _dir) = setup().await;

        assert!(!repo.is_completed("cleanup").await.unwrap());
        repo.mark_completed("cleanup").await.unwrap();
        assert!(repo.is_completed("cleanup").await.unwrap());
    }

    #[tokio::test]
    async fn mark_completed_is_idempotent() {
        let (repo, _dir) = setup().await;

        repo.mark_completed("cleanup").await.unwrap();
        repo.mark_completed("cleanup").await.unwrap();

        let completed_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM account_maintenance_tasks WHERE name = 'cleanup'",
        )
        .fetch_one(&repo.db.inner.pool)
        .await
        .unwrap();
        assert_eq!(completed_count, 1);
    }
}

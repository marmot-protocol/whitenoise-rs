use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        22
    }

    fn description(&self) -> &'static str {
        "Move account_follows into per-account database"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        // Per-account schema: store the followed pubkey directly. No FK to
        // users — user records live in the shared DB and are looked up
        // separately when a richer view is needed.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS account_follows (
                pubkey      TEXT NOT NULL PRIMARY KEY,
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await?;

        // Copy rows by joining shared.account_follows with shared.users to
        // recover the followed pubkey, filtering by this account.
        let shared_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='account_follows')",
        )
        .fetch_one(global_db)
        .await?;

        if !shared_table_exists {
            tracing::warn!(
                target: "whitenoise::database::rust_migrations::m0020",
                account = account_pubkey,
                "shared.account_follows is missing; skipping local copy"
            );
            return Ok(());
        }

        type Row = (String, i64, i64);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT u.pubkey, af.created_at, af.updated_at \
             FROM account_follows af \
             JOIN users u ON af.user_id = u.id \
             WHERE af.account_id = (SELECT id FROM accounts WHERE pubkey = ?)",
        )
        .bind(account_pubkey)
        .fetch_all(global_db)
        .await?;

        for (pubkey, created_at, updated_at) in rows {
            sqlx::query(
                "INSERT OR IGNORE INTO account_follows (pubkey, created_at, updated_at) \
                 VALUES (?, ?, ?)",
            )
            .bind(pubkey)
            .bind(created_at)
            .bind(updated_at)
            .execute(&mut *tx)
            .await?;
        }

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

    async fn seed_global(global: &SqlitePool) {
        sqlx::raw_sql(
            "CREATE TABLE accounts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pubkey TEXT NOT NULL UNIQUE
             );
             CREATE TABLE users (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pubkey TEXT NOT NULL UNIQUE
             );
             CREATE TABLE account_follows (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id INTEGER NOT NULL,
                user_id INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE(account_id, user_id)
             );",
        )
        .execute(global)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn migration_copies_followed_pubkeys() {
        let (local, global, _dir) = pools().await;
        let me_pubkey = "ab".repeat(32);
        let other_pubkey = "cd".repeat(32);
        let followed_a = "11".repeat(32);
        let followed_b = "22".repeat(32);
        let followed_c = "33".repeat(32);

        seed_global(&global).await;

        sqlx::query("INSERT INTO accounts (pubkey) VALUES (?), (?)")
            .bind(&me_pubkey)
            .bind(&other_pubkey)
            .execute(&global)
            .await
            .unwrap();
        sqlx::query("INSERT INTO users (pubkey) VALUES (?), (?), (?)")
            .bind(&followed_a)
            .bind(&followed_b)
            .bind(&followed_c)
            .execute(&global)
            .await
            .unwrap();

        // me follows A and B; other follows C.
        sqlx::query(
            "INSERT INTO account_follows (account_id, user_id, created_at, updated_at) \
             SELECT a.id, u.id, 100, 200 \
             FROM accounts a, users u \
             WHERE a.pubkey = ? AND u.pubkey = ?",
        )
        .bind(&me_pubkey)
        .bind(&followed_a)
        .execute(&global)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO account_follows (account_id, user_id, created_at, updated_at) \
             SELECT a.id, u.id, 110, 210 \
             FROM accounts a, users u \
             WHERE a.pubkey = ? AND u.pubkey = ?",
        )
        .bind(&me_pubkey)
        .bind(&followed_b)
        .execute(&global)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO account_follows (account_id, user_id, created_at, updated_at) \
             SELECT a.id, u.id, 120, 220 \
             FROM accounts a, users u \
             WHERE a.pubkey = ? AND u.pubkey = ?",
        )
        .bind(&other_pubkey)
        .bind(&followed_c)
        .execute(&global)
        .await
        .unwrap();

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &me_pubkey)))
            .await
            .unwrap();

        let rows: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT pubkey, created_at, updated_at FROM account_follows ORDER BY pubkey",
        )
        .fetch_all(&local)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, followed_a);
        assert_eq!(rows[0].1, 100);
        assert_eq!(rows[0].2, 200);
        assert_eq!(rows[1].0, followed_b);
    }

    #[tokio::test]
    async fn migration_tolerates_missing_shared_table() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ef".repeat(32);

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='account_follows')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);
    }
}

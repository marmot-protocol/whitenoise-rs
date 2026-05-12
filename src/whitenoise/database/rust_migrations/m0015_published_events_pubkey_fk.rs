use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        15
    }

    fn description(&self) -> &'static str {
        "Switch published_events.account_id (int FK) to account_pubkey (text FK)"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        sqlx::query(
            "CREATE TABLE published_events_new (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id        TEXT NOT NULL
                    CHECK (length(event_id) = 64 AND event_id NOT GLOB '*[^0-9a-fA-F]*'),
                account_pubkey  TEXT NOT NULL,
                created_at      DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE,
                UNIQUE(event_id, account_pubkey)
            )",
        )
        .execute(&mut *tx)
        .await?;

        // Snapshot row counts so we can log how many were translated vs.
        // dropped as orphans (rows whose `account_id` had no matching
        // `accounts.id` row — should be 0 in practice because the original
        // FK had ON DELETE CASCADE, but log if it ever happens).
        let original_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM published_events")
            .fetch_one(&mut *tx)
            .await?;

        // LEFT JOIN preserves row count visibility; the WHERE filter then
        // excludes orphans cleanly so the new table's NOT NULL constraint on
        // account_pubkey is respected.
        sqlx::query(
            "INSERT INTO published_events_new (id, event_id, account_pubkey, created_at)
             SELECT pe.id, pe.event_id, a.pubkey, pe.created_at
             FROM published_events pe
             LEFT JOIN accounts a ON a.id = pe.account_id
             WHERE a.pubkey IS NOT NULL",
        )
        .execute(&mut *tx)
        .await?;

        let copied_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM published_events_new")
            .fetch_one(&mut *tx)
            .await?;
        let dropped = original_count - copied_count;
        if dropped > 0 {
            tracing::warn!(
                target: "whitenoise::database::rust_migrations::m0013",
                "Dropped {dropped} orphaned published_events row(s) with no matching account \
                 (out of {original_count} total)"
            );
        }

        sqlx::query("DROP TABLE published_events")
            .execute(&mut *tx)
            .await?;

        sqlx::query("ALTER TABLE published_events_new RENAME TO published_events")
            .execute(&mut *tx)
            .await?;

        sqlx::query("CREATE INDEX idx_published_events_lookup ON published_events(event_id)")
            .execute(&mut *tx)
            .await?;

        sqlx::query(
            "CREATE INDEX idx_published_events_account_pubkey
                ON published_events(account_pubkey)",
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

    async fn pool() -> (SqlitePool, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("m0013.db");
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let pool = SqlitePool::connect(&url).await.unwrap();
        (pool, dir)
    }

    /// Seed a minimal pre-migration schema with one account row and one
    /// published_events row keyed by integer account_id.
    async fn seed_pre_migration(pool: &SqlitePool) -> String {
        sqlx::raw_sql(
            "CREATE TABLE accounts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pubkey TEXT NOT NULL UNIQUE
            );
             CREATE TABLE published_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL
                    CHECK (length(event_id) = 64 AND event_id NOT GLOB '*[^0-9a-fA-F]*'),
                account_id INTEGER NOT NULL,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE,
                UNIQUE(event_id, account_id)
             );",
        )
        .execute(pool)
        .await
        .unwrap();

        let pubkey = "ab".repeat(32);
        sqlx::query("INSERT INTO accounts (pubkey) VALUES (?)")
            .bind(&pubkey)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO published_events (event_id, account_id) VALUES (?, 1)")
            .bind("cd".repeat(32))
            .execute(pool)
            .await
            .unwrap();

        pubkey
    }

    #[tokio::test]
    async fn migration_translates_account_id_to_pubkey() {
        let (pool, _dir) = pool().await;
        let pubkey = seed_pre_migration(&pool).await;

        Migrator::new(vec![Box::new(Migration)], vec![])
            .run(&pool, None)
            .await
            .unwrap();

        // Old column gone, new column populated with the joined pubkey.
        let (account_pubkey,): (String,) =
            sqlx::query_as("SELECT account_pubkey FROM published_events")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(account_pubkey, pubkey);

        let has_old: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('published_events') WHERE name = 'account_id'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(has_old, 0, "account_id column should be gone");
    }
}

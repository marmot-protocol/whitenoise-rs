use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        14
    }

    fn description(&self) -> &'static str {
        "Switch processed_events.account_id (int FK, nullable) to account_pubkey (text FK)"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        sqlx::query(
            "CREATE TABLE processed_events_new (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id            TEXT NOT NULL
                    CHECK (length(event_id) = 64 AND event_id NOT GLOB '*[^0-9a-fA-F]*'),
                account_pubkey      TEXT,
                created_at          DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                event_created_at    INTEGER DEFAULT NULL,
                event_kind          INTEGER DEFAULT NULL,
                author              TEXT DEFAULT NULL,
                FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE,
                UNIQUE(event_id, account_pubkey)
            )",
        )
        .execute(&mut *tx)
        .await?;

        // LEFT JOIN preserves rows where account_id IS NULL (global-scoped).
        sqlx::query(
            "INSERT INTO processed_events_new
                (id, event_id, account_pubkey, created_at,
                 event_created_at, event_kind, author)
             SELECT pe.id, pe.event_id, a.pubkey, pe.created_at,
                    pe.event_created_at, pe.event_kind, pe.author
             FROM processed_events pe
             LEFT JOIN accounts a ON a.id = pe.account_id",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query("DROP TABLE processed_events")
            .execute(&mut *tx)
            .await?;

        sqlx::query("ALTER TABLE processed_events_new RENAME TO processed_events")
            .execute(&mut *tx)
            .await?;

        sqlx::query("CREATE INDEX idx_processed_events_lookup ON processed_events(event_id)")
            .execute(&mut *tx)
            .await?;

        sqlx::query(
            "CREATE INDEX idx_processed_events_account_pubkey
                ON processed_events(account_pubkey)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE UNIQUE INDEX idx_processed_events_global_unique
                ON processed_events(event_id)
             WHERE account_pubkey IS NULL",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX idx_processed_events_account_kind_timestamp
                ON processed_events(account_pubkey, event_kind, event_created_at)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX idx_processed_events_author_kind_timestamp
                ON processed_events(author, event_kind, event_created_at)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX idx_processed_events_null_account_kind_timestamp
                ON processed_events(event_kind, event_created_at)
             WHERE account_pubkey IS NULL",
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
        let path = dir.path().join("m0014.db");
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let pool = SqlitePool::connect(&url).await.unwrap();
        (pool, dir)
    }

    async fn seed_pre_migration(pool: &SqlitePool) -> String {
        sqlx::raw_sql(
            "CREATE TABLE accounts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pubkey TEXT NOT NULL UNIQUE
            );
             CREATE TABLE processed_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL
                    CHECK (length(event_id) = 64 AND event_id NOT GLOB '*[^0-9a-fA-F]*'),
                account_id INTEGER,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                event_created_at INTEGER DEFAULT NULL,
                event_kind INTEGER DEFAULT NULL,
                author TEXT DEFAULT NULL,
                FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE,
                UNIQUE(event_id, account_id)
             );
             CREATE UNIQUE INDEX idx_processed_events_global_unique
                ON processed_events(event_id) WHERE account_id IS NULL;",
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

        // Account-scoped row.
        sqlx::query(
            "INSERT INTO processed_events (event_id, account_id, event_kind) VALUES (?, 1, 1)",
        )
        .bind("cd".repeat(32))
        .execute(pool)
        .await
        .unwrap();

        // Global row (account_id NULL).
        sqlx::query(
            "INSERT INTO processed_events (event_id, account_id, event_kind) VALUES (?, NULL, 0)",
        )
        .bind("ef".repeat(32))
        .execute(pool)
        .await
        .unwrap();

        pubkey
    }

    #[tokio::test]
    async fn migration_preserves_global_and_account_rows() {
        let (pool, _dir) = pool().await;
        let pubkey = seed_pre_migration(&pool).await;

        Migrator::new(vec![Box::new(Migration)], vec![])
            .run(&pool, None)
            .await
            .unwrap();

        // Account-scoped row picked up the pubkey.
        let (account_pubkey,): (String,) =
            sqlx::query_as("SELECT account_pubkey FROM processed_events WHERE event_kind = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(account_pubkey, pubkey);

        // Global row stays NULL.
        let global: Option<String> =
            sqlx::query_scalar("SELECT account_pubkey FROM processed_events WHERE event_kind = 0")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            global.is_none(),
            "global row's account_pubkey should be NULL"
        );

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM processed_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2, "both rows should be preserved");
    }
}

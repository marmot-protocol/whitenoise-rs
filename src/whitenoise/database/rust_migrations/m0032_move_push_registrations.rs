use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

/// Temporary carrier for rows read from the shared `push_registrations` table
/// during migration. Not used outside this module.
#[derive(sqlx::FromRow)]
struct SharedPushRegRow {
    platform: String,
    raw_token: String,
    server_pubkey: String,
    relay_hint: Option<String>,
    created_at: i64,
    updated_at: i64,
    last_shared_at: Option<i64>,
}

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        32
    }

    fn description(&self) -> &'static str {
        "Move push_registrations into per-account database"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        // Per-account schema: no account_pubkey column (the file is the scope).
        // The UNIQUE index on account_pubkey is unnecessary — one DB = one
        // account, and there is only one row.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS push_registrations (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                platform        TEXT NOT NULL CHECK (platform IN ('apns', 'fcm')),
                raw_token       TEXT NOT NULL CHECK (
                    length(trim(raw_token, ' ' || char(9) || char(10) || char(13))) > 0
                ),
                server_pubkey   TEXT NOT NULL,
                relay_hint      TEXT,
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL,
                last_shared_at  INTEGER
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_push_registrations_server_pubkey
                ON push_registrations(server_pubkey)",
        )
        .execute(&mut *tx)
        .await?;

        let shared_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='push_registrations')",
        )
        .fetch_one(global_db)
        .await?;

        if !shared_table_exists {
            tracing::warn!(
                target: "whitenoise::database::rust_migrations::m0032",
                account = account_pubkey,
                "shared.push_registrations is missing; skipping local copy. \
                 Expected only on fresh installs or when v33 already dropped \
                 the table."
            );
            return Ok(());
        }

        // Read rows from shared via the global_db pool, then insert into the
        // per-account transaction. Can't use cross-DB SQL because the local
        // migration runs against the per-account pool.
        let rows: Vec<SharedPushRegRow> = sqlx::query_as(
            "SELECT platform, raw_token, server_pubkey, relay_hint,
                    created_at, updated_at, last_shared_at
             FROM push_registrations
             WHERE account_pubkey = ?",
        )
        .bind(account_pubkey)
        .fetch_all(global_db)
        .await?;

        for r in &rows {
            sqlx::query(
                "INSERT OR IGNORE INTO push_registrations
                    (platform, raw_token, server_pubkey, relay_hint,
                     created_at, updated_at, last_shared_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&r.platform)
            .bind(&r.raw_token)
            .bind(&r.server_pubkey)
            .bind(&r.relay_hint)
            .bind(r.created_at)
            .bind(r.updated_at)
            .bind(r.last_shared_at)
            .execute(&mut *tx)
            .await?;
        }

        if !rows.is_empty() {
            tracing::info!(
                target: "whitenoise::database::rust_migrations::m0032",
                account = account_pubkey,
                "Copied {} push_registrations row(s) to per-account DB",
                rows.len()
            );
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

    async fn seed_global(global: &SqlitePool, account_pubkey: &str) {
        sqlx::query(
            "CREATE TABLE push_registrations (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey  TEXT NOT NULL,
                platform        TEXT NOT NULL CHECK (platform IN ('apns', 'fcm')),
                raw_token       TEXT NOT NULL,
                server_pubkey   TEXT NOT NULL,
                relay_hint      TEXT,
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL,
                last_shared_at  INTEGER,
                UNIQUE(account_pubkey)
            )",
        )
        .execute(global)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO push_registrations
                (account_pubkey, platform, raw_token, server_pubkey, relay_hint,
                 created_at, updated_at, last_shared_at)
             VALUES (?, 'apns', 'token-abc', 'serverpub1', 'wss://relay.example.com',
                     1000, 2000, 3000)",
        )
        .bind(account_pubkey)
        .execute(global)
        .await
        .unwrap();

        // Row for a different account — should NOT be copied.
        sqlx::query(
            "INSERT INTO push_registrations
                (account_pubkey, platform, raw_token, server_pubkey,
                 created_at, updated_at)
             VALUES ('other_account', 'fcm', 'token-xyz', 'serverpub2', 4000, 5000)",
        )
        .execute(global)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn migration_copies_account_rows_to_local() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        seed_global(&global, &pubkey).await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM push_registrations")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(count, 1);

        let has_col: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('push_registrations') \
             WHERE name = 'account_pubkey'",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert_eq!(has_col, 0, "account_pubkey column should not exist");

        let (platform, raw_token, server_pubkey): (String, String, String) =
            sqlx::query_as("SELECT platform, raw_token, server_pubkey FROM push_registrations")
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(platform, "apns");
        assert_eq!(raw_token, "token-abc");
        assert_eq!(server_pubkey, "serverpub1");
    }

    #[tokio::test]
    async fn migration_creates_table_when_no_shared_rows() {
        let (local, global, _dir) = pools().await;
        let pubkey = "cd".repeat(32);

        sqlx::query(
            "CREATE TABLE push_registrations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey TEXT NOT NULL,
                platform TEXT NOT NULL,
                raw_token TEXT NOT NULL,
                server_pubkey TEXT NOT NULL,
                relay_hint TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                last_shared_at INTEGER,
                UNIQUE(account_pubkey)
            )",
        )
        .execute(&global)
        .await
        .unwrap();

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='push_registrations')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM push_registrations")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(count, 0);
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
             WHERE type='table' AND name='push_registrations')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);
    }
}

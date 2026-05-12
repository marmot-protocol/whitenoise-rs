use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

/// Temporary carrier for rows read from the shared `accounts_groups` table
/// during migration. Not used outside this module.
#[derive(sqlx::FromRow)]
struct SharedAccountGroupRow {
    mls_group_id: Vec<u8>,
    user_confirmation: Option<i64>,
    welcomer_pubkey: Option<String>,
    last_read_message_id: Option<String>,
    pin_order: Option<i64>,
    dm_peer_pubkey: Option<String>,
    archived_at: Option<i64>,
    removed_at: Option<i64>,
    self_removed: i64,
    muted_until: Option<i64>,
    chat_cleared_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        36
    }

    fn description(&self) -> &'static str {
        "Move accounts_groups into per-account database"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        // Per-account schema: no account_pubkey column (the file is the scope).
        // UNIQUE index changes from (account_pubkey, mls_group_id) to just
        // (mls_group_id). FK to accounts(pubkey) is dropped (cross-DB FK).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS accounts_groups (
                id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                mls_group_id         BLOB NOT NULL UNIQUE,
                user_confirmation    INTEGER DEFAULT NULL
                    CHECK (user_confirmation IS NULL OR user_confirmation IN (0, 1)),
                welcomer_pubkey      TEXT,
                last_read_message_id TEXT
                    CHECK (last_read_message_id IS NULL OR
                           (length(last_read_message_id) = 64
                            AND last_read_message_id NOT GLOB '*[^0-9a-fA-F]*')),
                pin_order            INTEGER DEFAULT NULL,
                dm_peer_pubkey       TEXT DEFAULT NULL,
                archived_at          INTEGER DEFAULT NULL,
                removed_at           INTEGER DEFAULT NULL,
                self_removed         INTEGER NOT NULL DEFAULT 0,
                muted_until          INTEGER DEFAULT NULL,
                chat_cleared_at      INTEGER DEFAULT NULL,
                created_at           INTEGER NOT NULL,
                updated_at           INTEGER NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_accounts_groups_group
                ON accounts_groups(mls_group_id)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_accounts_groups_dm_peer
                ON accounts_groups(dm_peer_pubkey)
                WHERE dm_peer_pubkey IS NOT NULL",
        )
        .execute(&mut *tx)
        .await?;

        let shared_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='accounts_groups')",
        )
        .fetch_one(global_db)
        .await?;

        if !shared_table_exists {
            tracing::warn!(
                target: "whitenoise::database::rust_migrations::m0036",
                account = account_pubkey,
                "shared.accounts_groups is missing; skipping local copy. \
                 Expected only on fresh installs or when v37 already dropped \
                 the table."
            );
            return Ok(());
        }

        let rows: Vec<SharedAccountGroupRow> = sqlx::query_as(
            "SELECT mls_group_id, user_confirmation, welcomer_pubkey,
                    last_read_message_id, pin_order, dm_peer_pubkey,
                    archived_at, removed_at, self_removed, muted_until,
                    chat_cleared_at, created_at, updated_at
             FROM accounts_groups
             WHERE account_pubkey = ?",
        )
        .bind(account_pubkey)
        .fetch_all(global_db)
        .await?;

        for r in &rows {
            sqlx::query(
                "INSERT OR IGNORE INTO accounts_groups
                    (mls_group_id, user_confirmation, welcomer_pubkey,
                     last_read_message_id, pin_order, dm_peer_pubkey,
                     archived_at, removed_at, self_removed, muted_until,
                     chat_cleared_at, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&r.mls_group_id)
            .bind(r.user_confirmation)
            .bind(&r.welcomer_pubkey)
            .bind(&r.last_read_message_id)
            .bind(r.pin_order)
            .bind(&r.dm_peer_pubkey)
            .bind(r.archived_at)
            .bind(r.removed_at)
            .bind(r.self_removed)
            .bind(r.muted_until)
            .bind(r.chat_cleared_at)
            .bind(r.created_at)
            .bind(r.updated_at)
            .execute(&mut *tx)
            .await?;
        }

        if !rows.is_empty() {
            tracing::info!(
                target: "whitenoise::database::rust_migrations::m0036",
                account = account_pubkey,
                "Copied {} accounts_groups row(s) to per-account DB",
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
            "CREATE TABLE accounts_groups (
                id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey       TEXT NOT NULL,
                mls_group_id         BLOB NOT NULL,
                user_confirmation    INTEGER DEFAULT NULL,
                welcomer_pubkey      TEXT,
                last_read_message_id TEXT,
                pin_order            INTEGER DEFAULT NULL,
                dm_peer_pubkey       TEXT DEFAULT NULL,
                archived_at          INTEGER DEFAULT NULL,
                removed_at           INTEGER DEFAULT NULL,
                self_removed         INTEGER NOT NULL DEFAULT 0,
                muted_until          INTEGER DEFAULT NULL,
                chat_cleared_at      INTEGER DEFAULT NULL,
                created_at           INTEGER NOT NULL,
                updated_at           INTEGER NOT NULL,
                UNIQUE(account_pubkey, mls_group_id)
            )",
        )
        .execute(global)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO accounts_groups
                (account_pubkey, mls_group_id, user_confirmation, welcomer_pubkey,
                 dm_peer_pubkey, created_at, updated_at)
             VALUES (?, X'01020304', 1, 'welcomer1', 'peer1', 1000, 2000)",
        )
        .bind(account_pubkey)
        .execute(global)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO accounts_groups
                (account_pubkey, mls_group_id, user_confirmation, pin_order,
                 created_at, updated_at)
             VALUES (?, X'05060708', 0, 42, 3000, 4000)",
        )
        .bind(account_pubkey)
        .execute(global)
        .await
        .unwrap();

        // Row for a different account — should NOT be copied.
        sqlx::query(
            "INSERT INTO accounts_groups
                (account_pubkey, mls_group_id, created_at, updated_at)
             VALUES ('other_account', X'09101112', 5000, 6000)",
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

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts_groups")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(count, 2);

        let has_col: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('accounts_groups') \
             WHERE name = 'account_pubkey'",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert_eq!(has_col, 0, "account_pubkey column should not exist");

        let (confirmation,): (Option<i64>,) = sqlx::query_as(
            "SELECT user_confirmation FROM accounts_groups WHERE mls_group_id = X'01020304'",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert_eq!(confirmation, Some(1));

        let (pin,): (Option<i64>,) = sqlx::query_as(
            "SELECT pin_order FROM accounts_groups WHERE mls_group_id = X'05060708'",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert_eq!(pin, Some(42));
    }

    #[tokio::test]
    async fn migration_creates_table_when_no_shared_rows() {
        let (local, global, _dir) = pools().await;
        let pubkey = "cd".repeat(32);

        sqlx::query(
            "CREATE TABLE accounts_groups (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey TEXT NOT NULL,
                mls_group_id BLOB NOT NULL,
                user_confirmation INTEGER DEFAULT NULL,
                welcomer_pubkey TEXT,
                last_read_message_id TEXT,
                pin_order INTEGER DEFAULT NULL,
                dm_peer_pubkey TEXT DEFAULT NULL,
                archived_at INTEGER DEFAULT NULL,
                removed_at INTEGER DEFAULT NULL,
                self_removed INTEGER NOT NULL DEFAULT 0,
                muted_until INTEGER DEFAULT NULL,
                chat_cleared_at INTEGER DEFAULT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE(account_pubkey, mls_group_id)
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
             WHERE type='table' AND name='accounts_groups')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts_groups")
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
             WHERE type='table' AND name='accounts_groups')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);
    }
}

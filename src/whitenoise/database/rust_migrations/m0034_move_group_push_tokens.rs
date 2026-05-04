use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

/// Temporary carrier for rows read from the shared `group_push_tokens` table
/// during migration. Not used outside this module.
#[derive(sqlx::FromRow)]
struct SharedGroupPushTokenRow {
    mls_group_id: Vec<u8>,
    member_pubkey: String,
    leaf_index: i64,
    server_pubkey: String,
    relay_hint: Option<String>,
    encrypted_token: String,
    created_at: i64,
    updated_at: i64,
}

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        34
    }

    fn description(&self) -> &'static str {
        "Move group_push_tokens into per-account database"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        // Per-account schema: no account_pubkey column (the file is the scope).
        // UNIQUE index changes from (account_pubkey, mls_group_id, leaf_index)
        // to (mls_group_id, leaf_index).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS group_push_tokens (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                mls_group_id    BLOB NOT NULL,
                member_pubkey   TEXT NOT NULL,
                leaf_index      INTEGER NOT NULL CHECK (leaf_index >= 0),
                server_pubkey   TEXT NOT NULL,
                relay_hint      TEXT,
                encrypted_token TEXT NOT NULL CHECK (
                    length(trim(encrypted_token, ' ' || char(9) || char(10) || char(13))) > 0
                ),
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_group_push_tokens_group_leaf
                ON group_push_tokens(mls_group_id, leaf_index)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_group_push_tokens_group
                ON group_push_tokens(mls_group_id)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_group_push_tokens_group_member
                ON group_push_tokens(mls_group_id, member_pubkey)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_group_push_tokens_server
                ON group_push_tokens(server_pubkey)",
        )
        .execute(&mut *tx)
        .await?;

        let shared_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='group_push_tokens')",
        )
        .fetch_one(global_db)
        .await?;

        if !shared_table_exists {
            tracing::warn!(
                target: "whitenoise::database::rust_migrations::m0034",
                account = account_pubkey,
                "shared.group_push_tokens is missing; skipping local copy. \
                 Expected only on fresh installs or when v35 already dropped \
                 the table."
            );
            return Ok(());
        }

        let rows: Vec<SharedGroupPushTokenRow> = sqlx::query_as(
            "SELECT mls_group_id, member_pubkey, leaf_index, server_pubkey,
                    relay_hint, encrypted_token, created_at, updated_at
             FROM group_push_tokens
             WHERE account_pubkey = ?",
        )
        .bind(account_pubkey)
        .fetch_all(global_db)
        .await?;

        for r in &rows {
            sqlx::query(
                "INSERT OR IGNORE INTO group_push_tokens
                    (mls_group_id, member_pubkey, leaf_index, server_pubkey,
                     relay_hint, encrypted_token, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&r.mls_group_id)
            .bind(&r.member_pubkey)
            .bind(r.leaf_index)
            .bind(&r.server_pubkey)
            .bind(&r.relay_hint)
            .bind(&r.encrypted_token)
            .bind(r.created_at)
            .bind(r.updated_at)
            .execute(&mut *tx)
            .await?;
        }

        if !rows.is_empty() {
            tracing::info!(
                target: "whitenoise::database::rust_migrations::m0034",
                account = account_pubkey,
                "Copied {} group_push_tokens row(s) to per-account DB",
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
            "CREATE TABLE group_push_tokens (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey  TEXT NOT NULL,
                mls_group_id    BLOB NOT NULL,
                member_pubkey   TEXT NOT NULL,
                leaf_index      INTEGER NOT NULL CHECK (leaf_index >= 0),
                server_pubkey   TEXT NOT NULL,
                relay_hint      TEXT,
                encrypted_token TEXT NOT NULL,
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL,
                UNIQUE(account_pubkey, mls_group_id, leaf_index)
            )",
        )
        .execute(global)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO group_push_tokens
                (account_pubkey, mls_group_id, member_pubkey, leaf_index,
                 server_pubkey, relay_hint, encrypted_token, created_at, updated_at)
             VALUES (?, X'01020304', 'member1', 3, 'serverpub1',
                     'wss://relay.example.com', 'cipher-abc', 1000, 2000)",
        )
        .bind(account_pubkey)
        .execute(global)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO group_push_tokens
                (account_pubkey, mls_group_id, member_pubkey, leaf_index,
                 server_pubkey, encrypted_token, created_at, updated_at)
             VALUES (?, X'01020304', 'member2', 7, 'serverpub2',
                     'cipher-def', 3000, 4000)",
        )
        .bind(account_pubkey)
        .execute(global)
        .await
        .unwrap();

        // Row for a different account — should NOT be copied.
        sqlx::query(
            "INSERT INTO group_push_tokens
                (account_pubkey, mls_group_id, member_pubkey, leaf_index,
                 server_pubkey, encrypted_token, created_at, updated_at)
             VALUES ('other_account', X'05060708', 'member3', 1, 'serverpub3',
                     'cipher-ghi', 5000, 6000)",
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

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM group_push_tokens")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(count, 2);

        let has_col: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('group_push_tokens') \
             WHERE name = 'account_pubkey'",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert_eq!(has_col, 0, "account_pubkey column should not exist");

        let (encrypted_token,): (String,) =
            sqlx::query_as("SELECT encrypted_token FROM group_push_tokens WHERE leaf_index = 3")
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(encrypted_token, "cipher-abc");
    }

    #[tokio::test]
    async fn migration_creates_table_when_no_shared_rows() {
        let (local, global, _dir) = pools().await;
        let pubkey = "cd".repeat(32);

        sqlx::query(
            "CREATE TABLE group_push_tokens (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey TEXT NOT NULL,
                mls_group_id BLOB NOT NULL,
                member_pubkey TEXT NOT NULL,
                leaf_index INTEGER NOT NULL,
                server_pubkey TEXT NOT NULL,
                relay_hint TEXT,
                encrypted_token TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE(account_pubkey, mls_group_id, leaf_index)
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
             WHERE type='table' AND name='group_push_tokens')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM group_push_tokens")
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
             WHERE type='table' AND name='group_push_tokens')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);
    }
}

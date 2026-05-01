use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        29
    }

    fn description(&self) -> &'static str {
        "Move media_references into per-account database"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        // Per-account schema: no account_pubkey column (the file is the scope).
        // UNIQUE constraint drops account_pubkey — it was only needed when
        // multiple accounts shared one table.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS media_references (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                mls_group_id        BLOB NOT NULL,
                encrypted_file_hash TEXT NOT NULL,
                media_type          TEXT NOT NULL,
                nostr_key           TEXT,
                file_metadata       BLOB,
                original_file_hash  TEXT,
                nonce               TEXT,
                scheme_version      TEXT,
                created_at          INTEGER NOT NULL,
                UNIQUE(mls_group_id, encrypted_file_hash)
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_media_refs_group_hash
                ON media_references(mls_group_id, encrypted_file_hash)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_media_refs_hash
                ON media_references(encrypted_file_hash)",
        )
        .execute(&mut *tx)
        .await?;

        let shared_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='media_references')",
        )
        .fetch_one(global_db)
        .await?;

        if !shared_table_exists {
            tracing::warn!(
                target: "whitenoise::database::rust_migrations::m0029",
                account = account_pubkey,
                "shared.media_references is missing; skipping local copy. \
                 Expected only on fresh installs or when v30 already dropped \
                 the table."
            );
            return Ok(());
        }

        // Read rows from shared via the global_db pool, then insert into the
        // per-account transaction. Can't use cross-DB SQL because the local
        // migration runs against the per-account pool.
        let rows: Vec<(
            Vec<u8>,         // mls_group_id
            String,          // encrypted_file_hash
            String,          // media_type
            Option<String>,  // nostr_key
            Option<Vec<u8>>, // file_metadata
            Option<String>,  // original_file_hash
            Option<String>,  // nonce
            Option<String>,  // scheme_version
            i64,             // created_at
        )> = sqlx::query_as(
            "SELECT mls_group_id, encrypted_file_hash, media_type, nostr_key,
                    file_metadata, original_file_hash, nonce, scheme_version, created_at
             FROM media_references
             WHERE account_pubkey = ?",
        )
        .bind(account_pubkey)
        .fetch_all(global_db)
        .await?;

        for row in &rows {
            sqlx::query(
                "INSERT OR IGNORE INTO media_references
                    (mls_group_id, encrypted_file_hash, media_type, nostr_key,
                     file_metadata, original_file_hash, nonce, scheme_version, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&row.0)
            .bind(&row.1)
            .bind(&row.2)
            .bind(&row.3)
            .bind(&row.4)
            .bind(&row.5)
            .bind(&row.6)
            .bind(&row.7)
            .bind(row.8)
            .execute(&mut *tx)
            .await?;
        }

        if !rows.is_empty() {
            tracing::info!(
                target: "whitenoise::database::rust_migrations::m0029",
                account = account_pubkey,
                "Copied {} media_references row(s) to per-account DB",
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
            "CREATE TABLE media_references (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                mls_group_id        BLOB NOT NULL,
                account_pubkey      TEXT NOT NULL,
                encrypted_file_hash TEXT NOT NULL,
                media_type          TEXT NOT NULL,
                nostr_key           TEXT,
                file_metadata       BLOB,
                original_file_hash  TEXT,
                nonce               TEXT,
                scheme_version      TEXT,
                created_at          INTEGER NOT NULL,
                UNIQUE(mls_group_id, encrypted_file_hash, account_pubkey)
            )",
        )
        .execute(global)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO media_references
                (mls_group_id, account_pubkey, encrypted_file_hash,
                 media_type, original_file_hash, created_at)
             VALUES (x'deadbeef', ?, 'hash1', 'chat_media', 'orig1', 1000)",
        )
        .bind(account_pubkey)
        .execute(global)
        .await
        .unwrap();

        // Row for a different account — should NOT be copied.
        sqlx::query(
            "INSERT INTO media_references
                (mls_group_id, account_pubkey, encrypted_file_hash,
                 media_type, created_at)
             VALUES (x'deadbeef', 'other_account', 'hash2', 'group_image', 2000)",
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

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM media_references")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(count, 1);

        let has_col: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('media_references') \
             WHERE name = 'account_pubkey'",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert_eq!(has_col, 0, "account_pubkey column should not exist");

        let (hash,): (String,) = sqlx::query_as("SELECT encrypted_file_hash FROM media_references")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(hash, "hash1");
    }

    #[tokio::test]
    async fn migration_creates_table_when_no_shared_rows() {
        let (local, global, _dir) = pools().await;
        let pubkey = "cd".repeat(32);

        sqlx::query(
            "CREATE TABLE media_references (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                mls_group_id BLOB NOT NULL,
                account_pubkey TEXT NOT NULL,
                encrypted_file_hash TEXT NOT NULL,
                media_type TEXT NOT NULL,
                nostr_key TEXT,
                file_metadata BLOB,
                original_file_hash TEXT,
                nonce TEXT,
                scheme_version TEXT,
                created_at INTEGER NOT NULL,
                UNIQUE(mls_group_id, encrypted_file_hash, account_pubkey)
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
             WHERE type='table' AND name='media_references')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM media_references")
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
             WHERE type='table' AND name='media_references')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);
    }
}

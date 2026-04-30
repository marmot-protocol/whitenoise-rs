use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        13
    }

    fn description(&self) -> &'static str {
        "Split media_files into media_blobs (shared) and media_references (account-scoped)"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        // 1. Create shared blob cache table, deduplicated by encrypted hash.
        sqlx::query(
            "CREATE TABLE media_blobs (
                encrypted_file_hash TEXT NOT NULL PRIMARY KEY,
                file_path           TEXT NOT NULL DEFAULT '',
                mime_type           TEXT NOT NULL,
                blossom_url         TEXT,
                created_at          INTEGER NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX idx_media_blobs_blossom_url
                ON media_blobs(blossom_url)
                WHERE blossom_url IS NOT NULL",
        )
        .execute(&mut *tx)
        .await?;

        // 2. Create account-scoped media reference table.
        //
        //    Logically this belongs in the per-account database, but
        //    AccountDatabase is not wired into production init yet (Phase 18a).
        //    All production queries JOIN media_references with media_blobs on
        //    a single pool, so the table must live on shared until the
        //    account-DB wiring lands (Phase 18c+). At that point a local
        //    migration extracts it and a follow-up global drops it from shared.
        //
        //    The FK to media_blobs is omitted: once this table moves to the
        //    account DB the FK would be cross-database and unenforced anyway.
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
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX idx_media_refs_account
                ON media_references(account_pubkey)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX idx_media_refs_group_hash
                ON media_references(mls_group_id, encrypted_file_hash)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX idx_media_refs_created
                ON media_references(created_at)",
        )
        .execute(&mut *tx)
        .await?;

        // 3. Backfill media_blobs from media_files (deduplicated by hash).
        //    Takes the first occurrence's file_path, mime_type, blossom_url,
        //    and earliest created_at for each unique encrypted_file_hash.
        sqlx::query(
            "INSERT INTO media_blobs
                (encrypted_file_hash, file_path, mime_type, blossom_url, created_at)
            SELECT
                encrypted_file_hash,
                COALESCE(
                    NULLIF(
                        (SELECT mf2.file_path
                         FROM media_files mf2
                         WHERE mf2.encrypted_file_hash = mf.encrypted_file_hash
                           AND mf2.file_path != ''
                         LIMIT 1),
                        NULL
                    ),
                    ''
                ),
                (SELECT mf3.mime_type
                 FROM media_files mf3
                 WHERE mf3.encrypted_file_hash = mf.encrypted_file_hash
                 LIMIT 1),
                (SELECT mf4.blossom_url
                 FROM media_files mf4
                 WHERE mf4.encrypted_file_hash = mf.encrypted_file_hash
                   AND mf4.blossom_url IS NOT NULL
                 LIMIT 1),
                MIN(created_at)
            FROM media_files mf
            GROUP BY encrypted_file_hash",
        )
        .execute(&mut *tx)
        .await?;

        // 4. Backfill media_references from media_files.
        sqlx::query(
            "INSERT INTO media_references
                (mls_group_id, account_pubkey, encrypted_file_hash,
                 media_type, nostr_key, file_metadata,
                 original_file_hash, nonce, scheme_version, created_at)
            SELECT
                mls_group_id, account_pubkey, encrypted_file_hash,
                media_type, nostr_key, file_metadata,
                original_file_hash, nonce, scheme_version, created_at
            FROM media_files",
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
    use crate::whitenoise::database::rust_migrations::{Migrator, all_global_migrations};

    async fn create_pool(dir: &TempDir, name: &str) -> SqlitePool {
        let path = dir.path().join(name);
        let url = format!("sqlite://{}?mode=rwc", path.display());
        SqlitePool::connect(&url).await.unwrap()
    }

    #[tokio::test]
    async fn backfill_deduplicates_blobs_and_preserves_references() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "backfill.db").await;

        // Run m0001-m0011 to get the pre-split schema (includes media_files).
        let pre_split: Vec<Box<dyn crate::whitenoise::database::rust_migrations::GlobalMigration>> =
            all_global_migrations()
                .into_iter()
                .filter(|m| m.version() < 13)
                .collect();
        let runner = Migrator::new(pre_split, vec![]);
        runner.run(&pool, None).await.unwrap();

        // Seed accounts to satisfy foreign keys.
        let acct_a = "aa".repeat(32);
        let acct_b = "bb".repeat(32);
        for pubkey in [&acct_a, &acct_b] {
            sqlx::query(
                "INSERT INTO users (pubkey, created_at, updated_at) VALUES (?, 1000, 1000)",
            )
            .bind(pubkey)
            .execute(&pool)
            .await
            .unwrap();
            let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE pubkey = ?")
                .bind(pubkey)
                .fetch_one(&pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO accounts (pubkey, user_id, created_at, updated_at) \
                 VALUES (?, ?, 1000, 1000)",
            )
            .bind(pubkey)
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();
        }

        // Seed media_files with 3 rows: two share the same encrypted_file_hash
        // (different accounts), one has a unique hash.
        let shared_hash = "aabbccdd";
        let unique_hash = "11223344";
        let group_id = vec![1u8; 8];

        for (account, hash, file_path, blossom_url) in [
            (
                acct_a.as_str(),
                shared_hash,
                "/path/a.enc",
                Some("https://blob.a"),
            ),
            (acct_b.as_str(), shared_hash, "/path/b.enc", None),
            (
                acct_a.as_str(),
                unique_hash,
                "/path/c.enc",
                Some("https://blob.c"),
            ),
        ] {
            sqlx::query(
                "INSERT INTO media_files
                    (mls_group_id, account_pubkey, file_path, original_file_hash,
                     encrypted_file_hash, mime_type, media_type, blossom_url,
                     nostr_key, nonce, scheme_version, created_at)
                VALUES (?, ?, ?, NULL, ?, 'image/png', 'chat_media', ?, NULL, NULL, NULL, 1000)",
            )
            .bind(&group_id)
            .bind(account)
            .bind(file_path)
            .bind(hash)
            .bind(blossom_url)
            .execute(&pool)
            .await
            .unwrap();
        }

        // Run m0013 to split the table.
        let split_only = Migrator::new(vec![Box::new(Migration)], vec![]);
        split_only.run(&pool, None).await.unwrap();

        // media_blobs: 2 rows (deduplicated by hash).
        let blob_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_blobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(blob_count, 2, "should have 2 distinct blobs");

        // media_references: 3 rows (one per original media_files row).
        let ref_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_references")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(ref_count, 3, "should have 3 references");

        // The shared blob should pick acct_a's file_path (first writer).
        let (blob_path,): (String,) =
            sqlx::query_as("SELECT file_path FROM media_blobs WHERE encrypted_file_hash = ?")
                .bind(shared_hash)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(blob_path, "/path/a.enc", "first writer's path wins");

        // The shared blob should have acct_a's blossom_url (non-NULL wins).
        let (blob_blossom,): (Option<String>,) =
            sqlx::query_as("SELECT blossom_url FROM media_blobs WHERE encrypted_file_hash = ?")
                .bind(shared_hash)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            blob_blossom.as_deref(),
            Some("https://blob.a"),
            "non-NULL blossom_url should be picked"
        );

        // Both accounts should have references to the shared hash.
        let shared_refs: Vec<(String,)> = sqlx::query_as(
            "SELECT account_pubkey FROM media_references \
             WHERE encrypted_file_hash = ? ORDER BY account_pubkey",
        )
        .bind(shared_hash)
        .fetch_all(&pool)
        .await
        .unwrap();
        let accounts: Vec<&str> = shared_refs.iter().map(|(a,)| a.as_str()).collect();
        assert_eq!(accounts, vec![acct_a.as_str(), acct_b.as_str()]);

        // media_files should still exist (dropped by m0014).
        let tables: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='media_files'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            !tables.is_empty(),
            "media_files should still exist after m0013"
        );
    }
}

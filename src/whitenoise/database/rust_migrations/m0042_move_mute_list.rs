use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

/// Temporary carrier for rows read from the shared `mute_list` table during
/// migration. Account scoping happens in the WHERE clause, so the
/// `account_pubkey` column isn't materialised here.
#[derive(sqlx::FromRow)]
struct SharedMuteListRow {
    muted_pubkey: String,
    is_private: i64,
    created_at: i64,
}

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        42
    }

    fn description(&self) -> &'static str {
        "Move mute_list into per-account database"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        // Per-account schema drops the redundant `account_pubkey` column —
        // ownership is implicit in the file. UNIQUE collapses to muted_pubkey
        // alone since each account file holds only its own entries.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS mute_list (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                muted_pubkey  TEXT NOT NULL UNIQUE
                    CHECK (length(muted_pubkey) = 64
                           AND muted_pubkey GLOB '[0-9a-fA-F]*'),
                is_private    INTEGER NOT NULL DEFAULT 1
                    CHECK (is_private IN (0, 1)),
                created_at    INTEGER NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await?;

        let shared_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='mute_list')",
        )
        .fetch_one(global_db)
        .await?;

        if !shared_table_exists {
            tracing::warn!(
                target: "whitenoise::database::rust_migrations::m0042",
                account = account_pubkey,
                "shared.mute_list is missing; skipping local copy. \
                 Expected only on fresh installs."
            );
            return Ok(());
        }

        let rows: Vec<SharedMuteListRow> = sqlx::query_as(
            "SELECT muted_pubkey, is_private, created_at
             FROM mute_list
             WHERE account_pubkey = ?",
        )
        .bind(account_pubkey)
        .fetch_all(global_db)
        .await?;

        let mut copied: usize = 0;
        let mut skipped: usize = 0;
        for r in &rows {
            let result = sqlx::query(
                "INSERT OR IGNORE INTO mute_list (muted_pubkey, is_private, created_at)
                 VALUES (?, ?, ?)",
            )
            .bind(&r.muted_pubkey)
            .bind(r.is_private)
            .bind(r.created_at)
            .execute(&mut *tx)
            .await?;

            if result.rows_affected() == 0 {
                skipped += 1;
            } else {
                copied += 1;
            }
        }

        if copied > 0 {
            tracing::info!(
                target: "whitenoise::database::rust_migrations::m0042",
                account = account_pubkey,
                "Copied {copied} mute_list row(s) to per-account DB",
            );
        }

        // CHECK / UNIQUE constraint failures get swallowed by INSERT OR IGNORE.
        // Surface the count so a corrupt-data scenario leaves a visible trail.
        if skipped > 0 {
            tracing::warn!(
                target: "whitenoise::database::rust_migrations::m0042",
                account = account_pubkey,
                "Skipped {skipped} mute_list row(s) during migration \
                 (constraint violation — likely malformed muted_pubkey)",
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

    async fn seed_shared(global: &SqlitePool) {
        sqlx::query(
            "CREATE TABLE mute_list (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey TEXT NOT NULL,
                muted_pubkey   TEXT NOT NULL,
                is_private     INTEGER NOT NULL DEFAULT 1,
                created_at     INTEGER NOT NULL,
                UNIQUE(account_pubkey, muted_pubkey)
            )",
        )
        .execute(global)
        .await
        .unwrap();
    }

    async fn insert_shared(
        global: &SqlitePool,
        account_pubkey: &str,
        muted_pubkey: &str,
        is_private: i64,
        created_at: i64,
    ) {
        sqlx::query(
            "INSERT INTO mute_list (account_pubkey, muted_pubkey, is_private, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(account_pubkey)
        .bind(muted_pubkey)
        .bind(is_private)
        .bind(created_at)
        .execute(global)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn migration_copies_only_owning_account_entries() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        let other = "cd".repeat(32);
        let target_a = "11".repeat(32);
        let target_b = "22".repeat(32);
        let target_c = "33".repeat(32);

        seed_shared(&global).await;
        insert_shared(&global, &pubkey, &target_a, 1, 1000).await;
        insert_shared(&global, &pubkey, &target_b, 0, 2000).await;
        insert_shared(&global, &other, &target_c, 1, 3000).await; // foreign

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mute_list")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(count, 2, "only the owning account's entries are copied");

        let (other_present,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM mute_list WHERE muted_pubkey = ?")
                .bind(&target_c)
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(
            other_present, 0,
            "foreign account's entries must not leak in"
        );

        let (priv_count,): (i64,) =
            sqlx::query_as("SELECT is_private FROM mute_list WHERE muted_pubkey = ?")
                .bind(&target_a)
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(priv_count, 1, "is_private value preserved");

        let (ts,): (i64,) =
            sqlx::query_as("SELECT created_at FROM mute_list WHERE muted_pubkey = ?")
                .bind(&target_b)
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(ts, 2000, "created_at value preserved");
    }

    #[tokio::test]
    async fn migration_creates_empty_table_when_no_entries() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ef".repeat(32);

        seed_shared(&global).await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='mute_list')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mute_list")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn migration_tolerates_missing_shared_table() {
        let (local, global, _dir) = pools().await;
        let pubkey = "12".repeat(32);

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='mute_list')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(
            exists,
            "local mute_list must be created even with no shared table"
        );
    }

    #[tokio::test]
    async fn migration_silently_skips_invalid_pubkey_rows() {
        // INSERT OR IGNORE swallows CHECK-constraint failures silently. In
        // practice every real `muted_pubkey` value comes from
        // `PublicKey::parse` so corrupt rows shouldn't exist, but if shared
        // ever contains junk we'd rather drop the row than fail startup.
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        let good_target = "11".repeat(32);
        let bad_target = "not-hex-pubkey".to_string();

        seed_shared(&global).await;
        insert_shared(&global, &pubkey, &bad_target, 1, 1000).await;
        insert_shared(&global, &pubkey, &good_target, 0, 2000).await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .expect("migration must succeed even when shared has corrupt rows");

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mute_list")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(count, 1, "only the well-formed row survives the CHECK");

        let (only,): (String,) = sqlx::query_as("SELECT muted_pubkey FROM mute_list")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(only, good_target);
    }
}

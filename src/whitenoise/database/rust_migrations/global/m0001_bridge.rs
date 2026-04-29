use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

/// The last SQLx migration version shipped in a released app build
/// (v2026.3.23 shipped 0037_metadata_expires_at). Existing installs
/// must be at this version; everything beyond is handled by Rust
/// migrations m0002-m0011. Never bump this constant.
const EXPECTED_SQLX_VERSION: i64 = 37;

/// Consolidated schema DDL for fresh installs (covers 0001-0037).
/// Subsequent schema changes come from Rust migrations m0002+.
const FRESH_SCHEMA_SQL: &str = include_str!("../fresh_schema.sql");

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        1
    }

    fn description(&self) -> &'static str {
        "Bridge from SQLx migrations to Rust-only"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        let sqlx_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='_sqlx_migrations')",
        )
        .fetch_one(&mut *tx)
        .await?;

        if sqlx_table_exists {
            // Existing install: verify SQLx migrations reached the last shipped version.
            let max_version: i64 =
                sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
                    .fetch_one(&mut *tx)
                    .await?;

            if max_version < EXPECTED_SQLX_VERSION {
                return Err(DatabaseError::Sqlx(sqlx::Error::Protocol(format!(
                    "SQLx migrations unexpected state: found version {max_version}, \
                     expected at least {EXPECTED_SQLX_VERSION}. \
                     Please update to v2026.3.23 first."
                ))));
            }

            // Auto-stamp Rust conversions whose SQLx ancestor is already applied.
            // Mapping: Rust v N (N >= 2) corresponds to SQLx version N + 36
            // (m0002 ↔ 0038, m0003 ↔ 0039, ..., m0011 ↔ 0047).
            // This prevents duplicate schema changes when a user upgrades
            // from a master release (which shipped SQLx 0038-0046) to a release
            // after the architecture refactor (which uses Rust migrations instead).
            let mut stamped = 0u32;
            for rust_version in 2..=11_i64 {
                let sqlx_equivalent = rust_version + 36;
                if sqlx_equivalent <= max_version {
                    sqlx::query(
                        "INSERT OR IGNORE INTO _rust_migrations \
                         (version, description) VALUES (?, ?)",
                    )
                    .bind(rust_version)
                    .bind(
                        "Auto-stamped: corresponding SQLx migration \
                         already applied",
                    )
                    .execute(&mut *tx)
                    .await?;
                    stamped += 1;
                }
            }

            if max_version > 47 {
                tracing::warn!(
                    target: "whitenoise::database::rust_migrations",
                    "SQLx version {max_version} exceeds expected maximum (47); \
                     all Rust conversions were auto-stamped"
                );
            }

            tracing::info!(
                target: "whitenoise::database::rust_migrations",
                "Existing install detected, SQLx migrations at v{max_version} \
                 (>= required v{EXPECTED_SQLX_VERSION}), \
                 auto-stamped {stamped} Rust conversion(s)"
            );
        } else {
            // Fresh install: create the base schema from scratch.
            sqlx::raw_sql(FRESH_SCHEMA_SQL).execute(&mut *tx).await?;

            tracing::info!(
                target: "whitenoise::database::rust_migrations",
                "Fresh install: created base schema from scratch"
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
    use crate::whitenoise::database::rust_migrations::GlobalMigrationRunner;
    use crate::whitenoise::database::rust_migrations::global::all_global_migrations;

    async fn create_pool(dir: &TempDir, name: &str) -> SqlitePool {
        let path = dir.path().join(name);
        let url = format!("sqlite://{}?mode=rwc", path.display());
        SqlitePool::connect(&url).await.unwrap()
    }

    /// Seed a fake `_sqlx_migrations` table with versions 1..=max_ver.
    async fn seed_sqlx_migrations(pool: &SqlitePool, max_ver: i64) {
        sqlx::query(
            "CREATE TABLE _sqlx_migrations (
                version INTEGER PRIMARY KEY,
                description TEXT NOT NULL,
                installed_on TEXT NOT NULL DEFAULT (datetime('now')),
                success INTEGER NOT NULL DEFAULT 1,
                checksum BLOB NOT NULL DEFAULT x'00',
                execution_time INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(pool)
        .await
        .unwrap();

        for v in 1..=max_ver {
            sqlx::query("INSERT INTO _sqlx_migrations (version, description) VALUES (?, ?)")
                .bind(v)
                .bind(format!("migration_{v:04}"))
                .execute(pool)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn fresh_install_creates_schema() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "fresh.db").await;

        let runner = GlobalMigrationRunner::new(vec![Box::new(Migration)]);
        runner.run(&pool).await.unwrap();

        // Verify core tables exist (spot-check across fresh_schema.sql).
        for table in &[
            "accounts",
            "users",
            "aggregated_messages",
            "media_files",
            "relay_status",
            "message_delivery_status",
            "published_key_packages",
        ] {
            let exists: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type='table' AND name='{table}')"
            )))
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(exists, "table '{table}' should exist after fresh install");
        }

        // No _sqlx_migrations table on fresh install.
        let sqlx_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='_sqlx_migrations')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            !sqlx_exists,
            "_sqlx_migrations should not exist on fresh install"
        );
    }

    #[tokio::test]
    async fn existing_install_with_correct_version_passes() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "existing.db").await;

        // Simulate a v2026.3.23 install (37 SQLx migrations applied).
        sqlx::raw_sql(FRESH_SCHEMA_SQL)
            .execute(&pool)
            .await
            .unwrap();
        seed_sqlx_migrations(&pool, EXPECTED_SQLX_VERSION).await;

        let runner = GlobalMigrationRunner::new(vec![Box::new(Migration)]);
        runner.run(&pool).await.unwrap();

        // Bridge should pass through without error; no auto-stamps at version 37.
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "only bridge v1 should be recorded");
    }

    #[tokio::test]
    async fn existing_install_with_wrong_version_errors() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "wrong_ver.db").await;

        // Simulate an install too old to bridge (version 10 < 37).
        seed_sqlx_migrations(&pool, 10).await;

        let migration = Migration;
        let mut conn = pool.acquire().await.unwrap();
        let result = migration.run_global(&mut conn).await;

        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("unexpected state"),
            "error should mention unexpected state, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn bridge_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "idempotent.db").await;

        let runner = GlobalMigrationRunner::new(vec![Box::new(Migration)]);

        runner.run(&pool).await.unwrap();
        runner.run(&pool).await.unwrap();

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn master_upgrade_stamps_existing_sqlx_conversions() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "master_upgrade.db").await;

        // Simulate a user who upgraded via master release (SQLx 0001-0046).
        sqlx::raw_sql(FRESH_SCHEMA_SQL)
            .execute(&pool)
            .await
            .unwrap();
        seed_sqlx_migrations(&pool, 46).await;

        // Apply the schema changes that SQLx 0038-0046 would have made
        // (fresh_schema only covers 0001-0037).
        sqlx::raw_sql(
            "ALTER TABLE accounts_groups ADD COLUMN removed_at INTEGER DEFAULT NULL;
             ALTER TABLE accounts_groups ADD COLUMN muted_until INTEGER DEFAULT NULL;
             ALTER TABLE accounts_groups ADD COLUMN self_removed INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE accounts_groups ADD COLUMN chat_cleared_at INTEGER DEFAULT NULL;
             CREATE TABLE IF NOT EXISTS push_registrations (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 account_pubkey TEXT NOT NULL,
                 platform TEXT NOT NULL,
                 raw_token TEXT NOT NULL,
                 server_pubkey TEXT NOT NULL,
                 relay_hint TEXT,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL,
                 last_shared_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS group_push_tokens (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 account_pubkey TEXT NOT NULL,
                 mls_group_id BLOB NOT NULL,
                 member_pubkey TEXT NOT NULL,
                 leaf_index INTEGER NOT NULL,
                 server_pubkey TEXT NOT NULL,
                 relay_hint TEXT,
                 encrypted_token TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS mute_list (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 account_pubkey TEXT NOT NULL,
                 muted_pubkey TEXT NOT NULL,
                 is_private INTEGER NOT NULL DEFAULT 1,
                 created_at INTEGER NOT NULL,
                 UNIQUE(account_pubkey, muted_pubkey)
             );
             ALTER TABLE published_key_packages ADD COLUMN kind INTEGER NOT NULL DEFAULT 443;
             ALTER TABLE published_key_packages ADD COLUMN d_tag TEXT NULL;",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Run the full migration framework.
        let runner = GlobalMigrationRunner::new(all_global_migrations());
        runner.run(&pool).await.unwrap();

        // Bridge (v1) + auto-stamped v2-v10 + actually-ran v11 = 11 total.
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 11, "should have all 11 migration versions recorded");

        // Verify v2-v10 are stamped (not executed).
        let stamped: Vec<(i64, String)> = sqlx::query_as(
            "SELECT version, description FROM _rust_migrations \
             WHERE version BETWEEN 2 AND 10 ORDER BY version",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(stamped.len(), 9);
        for (_, desc) in &stamped {
            assert!(
                desc.contains("Auto-stamped"),
                "v2-v10 should be auto-stamped, got: {desc}"
            );
        }

        // Verify m0011 actually ran (message_delivery_status has account_pubkey).
        let has_account_pubkey: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('message_delivery_status') \
             WHERE name = 'account_pubkey'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            has_account_pubkey, 1,
            "m0011 should have added account_pubkey to message_delivery_status"
        );
    }

    #[tokio::test]
    async fn version_37_stamps_nothing() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "v37.db").await;

        sqlx::raw_sql(FRESH_SCHEMA_SQL)
            .execute(&pool)
            .await
            .unwrap();
        seed_sqlx_migrations(&pool, 37).await;

        // Run full framework — all conversions should execute, none stamped.
        let runner = GlobalMigrationRunner::new(all_global_migrations());
        runner.run(&pool).await.unwrap();

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 11, "all 11 migrations should be recorded");

        // None should be auto-stamped.
        let stamped: Vec<(i64,)> = sqlx::query_as(
            "SELECT version FROM _rust_migrations \
             WHERE description LIKE '%Auto-stamped%'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            stamped.is_empty(),
            "no migrations should be auto-stamped at version 37"
        );
    }

    #[tokio::test]
    async fn partial_sqlx_version_stamps_subset() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "partial.db").await;

        // Simulate a user at SQLx version 42 (applied 0038-0042).
        sqlx::raw_sql(FRESH_SCHEMA_SQL)
            .execute(&pool)
            .await
            .unwrap();
        seed_sqlx_migrations(&pool, 42).await;

        // Apply schema changes for 0038-0042 only.
        sqlx::raw_sql(
            "ALTER TABLE accounts_groups ADD COLUMN removed_at INTEGER DEFAULT NULL;
             CREATE TABLE IF NOT EXISTS push_registrations (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 account_pubkey TEXT NOT NULL,
                 platform TEXT NOT NULL,
                 raw_token TEXT NOT NULL,
                 server_pubkey TEXT NOT NULL,
                 relay_hint TEXT,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL,
                 last_shared_at INTEGER
             );
             ALTER TABLE accounts_groups ADD COLUMN muted_until INTEGER DEFAULT NULL;
             CREATE TABLE IF NOT EXISTS group_push_tokens (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 account_pubkey TEXT NOT NULL,
                 mls_group_id BLOB NOT NULL,
                 member_pubkey TEXT NOT NULL,
                 leaf_index INTEGER NOT NULL,
                 server_pubkey TEXT NOT NULL,
                 relay_hint TEXT,
                 encrypted_token TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             ALTER TABLE accounts_groups ADD COLUMN self_removed INTEGER NOT NULL DEFAULT 0;",
        )
        .execute(&pool)
        .await
        .unwrap();

        let runner = GlobalMigrationRunner::new(all_global_migrations());
        runner.run(&pool).await.unwrap();

        // v2-v6 stamped (SQLx 38-42), v7-v11 executed.
        let stamped: Vec<(i64,)> = sqlx::query_as(
            "SELECT version FROM _rust_migrations \
             WHERE description LIKE '%Auto-stamped%' ORDER BY version",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let stamped_versions: Vec<i64> = stamped.iter().map(|(v,)| *v).collect();
        assert_eq!(
            stamped_versions,
            vec![2, 3, 4, 5, 6],
            "v2-v6 should be auto-stamped for SQLx 42"
        );

        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, 11, "all 11 migrations should be recorded");
    }
}

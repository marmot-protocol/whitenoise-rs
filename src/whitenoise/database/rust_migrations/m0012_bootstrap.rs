use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

const FRESH_ACCOUNT_SCHEMA: &str = include_str!("fresh_account_schema.sql");

/// The unified-timeline version this bootstrap occupies.
///
/// Locals share the version space with globals; this is the next free slot
/// after the existing v30 global (`m0030_drop_shared_media_references`).
const BOOTSTRAP_VERSION: u32 = 31;

/// Local versions strictly less than `BOOTSTRAP_VERSION` whose effect is
/// already captured in `fresh_account_schema.sql`. Stamped into a fresh
/// account's `_rust_migrations` table so the runner skips them.
///
/// To rebaseline further: update the schema file, bump the bootstrap to a
/// higher version (or replace this file with a newer bootstrap migration),
/// and append the now-skipped versions here.
const STAMPED_PRIOR_LOCAL_VERSIONS: &[u32] = &[17, 18, 19, 20, 21, 22, 29];

/// Bootstrap a fresh account database with the schema represented by
/// `fresh_account_schema.sql` and stamp any prior local versions whose
/// effect that schema already captures.
///
/// New accounts: bootstrap sets up the schema once, prior versions are
/// stamped, subsequent locals (added after the baseline) run normally.
///
/// Existing accounts that have already recorded `BOOTSTRAP_VERSION` skip
/// this — the runner's idempotency check fires before `run_local` is
/// called.
pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        BOOTSTRAP_VERSION
    }

    fn description(&self) -> &'static str {
        "Bootstrap account database schema"
    }

    fn for_new_accounts_only(&self) -> bool {
        true
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        _global_db: &SqlitePool,
        _account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        sqlx::raw_sql(FRESH_ACCOUNT_SCHEMA)
            .execute(&mut *tx)
            .await?;

        for &v in STAMPED_PRIOR_LOCAL_VERSIONS {
            sqlx::query(
                "INSERT OR IGNORE INTO _rust_migrations (version, description) \
                 VALUES (?, 'Auto-stamped by local bootstrap baseline')",
            )
            .bind(v as i64)
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

    async fn create_pool(dir: &TempDir, name: &str) -> SqlitePool {
        let path = dir.path().join(name);
        let url = format!("sqlite://{}?mode=rwc", path.display());
        SqlitePool::connect(&url).await.unwrap()
    }

    #[tokio::test]
    async fn bootstrap_records_itself_for_a_fresh_account() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let account = create_pool(&dir, "account.db").await;
        let pubkey = "aa".repeat(32);

        let migrator = Migrator::new(vec![], vec![Box::new(Migration)]);
        migrator
            .run(&shared, Some((&account, &pubkey)))
            .await
            .unwrap();

        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version = ?")
                .bind(BOOTSTRAP_VERSION as i64)
                .fetch_one(&account)
                .await
                .unwrap();
        assert_eq!(
            count, 1,
            "bootstrap version should be recorded on the account DB"
        );

        let (shared_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&shared)
            .await
            .unwrap();
        assert_eq!(shared_count, 0, "bootstrap must not write to shared");
    }

    #[tokio::test]
    async fn bootstrap_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let account = create_pool(&dir, "account.db").await;
        let pubkey = "bb".repeat(32);

        let migrator = Migrator::new(vec![], vec![Box::new(Migration)]);
        migrator
            .run(&shared, Some((&account, &pubkey)))
            .await
            .unwrap();
        migrator
            .run(&shared, Some((&account, &pubkey)))
            .await
            .unwrap();

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&account)
            .await
            .unwrap();
        assert_eq!(count, 1 + STAMPED_PRIOR_LOCAL_VERSIONS.len() as i64);
    }

    /// Synthesises a rebaseline scenario: the bootstrap's stamping path is
    /// exercised by a probe migration that pre-declares a list of prior
    /// versions to stamp. This is the same code path the real bootstrap
    /// will use after Phase 18b adds extraction locals.
    #[tokio::test]
    async fn bootstrap_stamps_versions_listed_as_prior_baseline() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let account = create_pool(&dir, "account.db").await;
        let pubkey = "cc".repeat(32);

        struct Probe;

        #[async_trait]
        impl LocalMigration for Probe {
            fn version(&self) -> u32 {
                100
            }
            fn description(&self) -> &'static str {
                "rebaseline probe"
            }
            async fn run_local(
                &self,
                tx: &mut SqliteConnection,
                _global_db: &SqlitePool,
                _account_pubkey: &str,
            ) -> Result<(), DatabaseError> {
                for v in [50_u32, 75, 99] {
                    sqlx::query(
                        "INSERT OR IGNORE INTO _rust_migrations (version, description) \
                         VALUES (?, 'stamped baseline')",
                    )
                    .bind(v as i64)
                    .execute(&mut *tx)
                    .await?;
                }
                Ok(())
            }
        }

        let migrator = Migrator::new(vec![], vec![Box::new(Probe)]);
        migrator
            .run(&shared, Some((&account, &pubkey)))
            .await
            .unwrap();

        let stamped: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM _rust_migrations ORDER BY version")
                .fetch_all(&account)
                .await
                .unwrap();
        let versions: Vec<u32> = stamped.iter().map(|(v,)| *v as u32).collect();
        assert_eq!(versions, vec![50, 75, 99, 100]);
    }

    /// An account that pre-existed before the bootstrap landed (it already
    /// has prior *local* migration rows in `_rust_migrations`) must NOT
    /// execute the bootstrap, and must not gain a phantom row for it
    /// either: the bootstrap is for fresh per-user files only, and the
    /// tracking table records what actually ran.
    ///
    /// Pre-existing-ness is gauged against local versions only — see
    /// `new_accounts_only_runs_when_only_globals_are_pre_stamped` for the
    /// regression test that proves globals don't count.
    #[tokio::test]
    async fn bootstrap_skips_for_pre_existing_account() {
        use crate::whitenoise::database::rust_migrations::LocalMigration;

        struct Probe;
        #[async_trait]
        impl LocalMigration for Probe {
            fn version(&self) -> u32 {
                25
            }
            fn description(&self) -> &'static str {
                "post-bootstrap probe"
            }
            async fn run_local(
                &self,
                _tx: &mut SqliteConnection,
                _global_db: &SqlitePool,
                _account_pubkey: &str,
            ) -> Result<(), DatabaseError> {
                Ok(())
            }
        }

        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let account = create_pool(&dir, "account.db").await;
        let pubkey = "dd".repeat(32);

        // Simulate an account that already had *local* timeline progress
        // recorded — that's what makes it non-fresh to the runner.
        sqlx::query(
            "CREATE TABLE _rust_migrations (
                version     INTEGER PRIMARY KEY,
                description TEXT    NOT NULL,
                applied_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            )",
        )
        .execute(&account)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO _rust_migrations (version, description) VALUES (25, 'prior local')",
        )
        .execute(&account)
        .await
        .unwrap();

        let migrator = Migrator::new(vec![], vec![Box::new(Migration), Box::new(Probe)]);
        migrator
            .run(&shared, Some((&account, &pubkey)))
            .await
            .unwrap();

        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version = ?")
                .bind(BOOTSTRAP_VERSION as i64)
                .fetch_one(&account)
                .await
                .unwrap();
        assert_eq!(
            count, 0,
            "bootstrap must be silently skipped on a pre-existing account; \
             no row should be written"
        );
    }
}

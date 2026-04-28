use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

/// The version number of the last SQLx migration file (0045_...).
/// Used to verify existing installs completed all SQL migrations.
/// Frozen at the time the framework PR ships -- never bump this.
/// If SQL migrations are added before this PR lands, update this
/// constant and regenerate fresh_schema.sql.
const EXPECTED_SQLX_VERSION: i64 = 45;

/// Consolidated schema DDL for fresh installs.
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
            // Existing install: verify the expected final migration version.
            // Any missing SQLx migrations were already caught up by the runner's
            // pre-step (see GlobalMigrationRunner::run).
            let max_version: i64 =
                sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
                    .fetch_one(&mut *tx)
                    .await?;

            if max_version != EXPECTED_SQLX_VERSION {
                return Err(DatabaseError::Sqlx(sqlx::Error::Protocol(format!(
                    "SQLx migrations unexpected state: found version {max_version}, \
                     expected {EXPECTED_SQLX_VERSION}"
                ))));
            }

            tracing::info!(
                target: "whitenoise::database::rust_migrations",
                "Existing install detected, SQLx migrations verified at v{EXPECTED_SQLX_VERSION}"
            );
        } else {
            // Fresh install: create the full schema from scratch.
            sqlx::raw_sql(FRESH_SCHEMA_SQL).execute(&mut *tx).await?;

            tracing::info!(
                target: "whitenoise::database::rust_migrations",
                "Fresh install: created schema from scratch"
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

    async fn create_pool(dir: &TempDir, name: &str) -> SqlitePool {
        let path = dir.path().join(name);
        let url = format!("sqlite://{}?mode=rwc", path.display());
        SqlitePool::connect(&url).await.unwrap()
    }

    #[tokio::test]
    async fn fresh_install_creates_schema() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "fresh.db").await;

        let runner = GlobalMigrationRunner::new(vec![Box::new(Migration)]);
        runner.run(&pool).await.unwrap();

        // Verify core tables exist.
        for table in &["accounts", "users", "aggregated_messages", "media_files"] {
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

        // Simulate an existing install by running the real SQLx migrator.
        crate::whitenoise::database::MIGRATOR
            .run(&pool)
            .await
            .unwrap();

        let runner = GlobalMigrationRunner::new(vec![Box::new(Migration)]);
        runner.run(&pool).await.unwrap();

        // Bridge should pass through without error.
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn existing_install_with_wrong_version_errors() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "wrong_ver.db").await;

        // Defensive test: directly invokes bridge with stale state to verify
        // the version-check assertion fires. Not reachable through normal
        // Runner::run flow — in production, the runner's MIGRATOR.run()
        // catch-up would either bring max_version to EXPECTED_SQLX_VERSION
        // or error before the bridge runs. We bypass the runner here to
        // isolate the bridge's own guard.
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
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("INSERT INTO _sqlx_migrations (version, description) VALUES (10, 'fake')")
            .execute(&pool)
            .await
            .unwrap();

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
}

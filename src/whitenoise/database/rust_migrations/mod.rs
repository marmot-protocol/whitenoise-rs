use std::time::Instant;

use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use super::DatabaseError;

pub mod global;
pub mod local;

/// A migration that runs against the global (shared) database only.
///
/// Use for: app-wide schema changes, global data transformations,
/// settings migrations.
#[async_trait]
pub trait GlobalMigration: Send + Sync {
    /// Unique, monotonically increasing version number.
    ///
    /// Numbering starts at 1 and increments by 1. Gaps are allowed but
    /// discouraged. The runner executes migrations in version order.
    fn version(&self) -> u32;

    /// Human-readable description for logging and the tracking table.
    fn description(&self) -> &'static str;

    /// Execute the migration inside an open transaction.
    ///
    /// The caller (runner) begins the transaction and passes it here.
    /// Return `Ok(())` on success. Any error rolls back the transaction and
    /// aborts startup.
    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError>;
}

/// A migration that runs against a per-account (local) database,
/// with read access to the global database.
///
/// Use for: Phase 18 data extraction (shared -> account), per-account
/// data transformations that need cross-DB reads.
#[async_trait]
pub trait LocalMigration: Send + Sync {
    /// Unique, monotonically increasing version number.
    ///
    /// Local migrations have their own version sequence, independent of
    /// global migration versions.
    fn version(&self) -> u32;

    /// Human-readable description for logging and the tracking table.
    fn description(&self) -> &'static str;

    /// Execute the migration inside an open transaction on the local DB.
    ///
    /// - `tx`: the per-account database transaction (writable)
    /// - `global_db`: the shared database pool (read-only within migration context)
    /// - `account_pubkey`: hex pubkey of the account that owns this local DB
    ///
    /// Return `Ok(())` on success. Any error rolls back the transaction and
    /// aborts startup.
    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError>;
}

const CREATE_TRACKING_TABLE: &str = "CREATE TABLE IF NOT EXISTS _rust_migrations (
        version     INTEGER PRIMARY KEY,
        description TEXT    NOT NULL,
        applied_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
    )";

pub struct GlobalMigrationRunner {
    migrations: Vec<Box<dyn GlobalMigration>>,
}

impl GlobalMigrationRunner {
    /// Create a runner with the given migrations.
    ///
    /// # Panics
    ///
    /// Panics if any two migrations share the same version number.
    pub fn new(migrations: Vec<Box<dyn GlobalMigration>>) -> Self {
        let mut seen = std::collections::HashSet::new();
        for m in &migrations {
            assert!(
                seen.insert(m.version()),
                "duplicate global migration version: {}",
                m.version()
            );
        }
        let mut runner = Self { migrations };
        runner.migrations.sort_by_key(|m| m.version());
        runner
    }

    /// Run all pending global migrations.
    ///
    /// 1. If `_sqlx_migrations` exists, runs `MIGRATOR.run()` to catch up
    ///    any missing SQLx migrations (idempotent, skipped for fresh installs).
    /// 2. Creates `_rust_migrations` tracking table if needed.
    /// 3. Applies pending Rust migrations in version order, each in its own
    ///    transaction.
    pub async fn run(&self, global_db: &SqlitePool) -> Result<(), DatabaseError> {
        let start = Instant::now();

        // Pre-step: catch up SQLx migrations for existing installs.
        let sqlx_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='_sqlx_migrations')",
        )
        .fetch_one(global_db)
        .await?;

        if sqlx_table_exists {
            super::MIGRATOR.run(global_db).await.map_err(|e| {
                tracing::error!(
                    target: "whitenoise::database::rust_migrations",
                    "SQLx catch-up failed (existing install may need manual intervention): {e}"
                );
                DatabaseError::Sqlx(e.into())
            })?;
        }

        // Ensure tracking table exists.
        sqlx::query(CREATE_TRACKING_TABLE)
            .execute(global_db)
            .await?;

        // Query already-applied versions.
        let applied: Vec<(i64,)> = sqlx::query_as("SELECT version FROM _rust_migrations")
            .fetch_all(global_db)
            .await?;
        let applied: std::collections::HashSet<u32> =
            applied.iter().map(|(v,)| *v as u32).collect();

        let pending: Vec<&dyn GlobalMigration> = self
            .migrations
            .iter()
            .filter(|m| !applied.contains(&m.version()))
            .map(|m| m.as_ref())
            .collect();

        if pending.is_empty() {
            return Ok(());
        }

        let mut count = 0u32;
        for migration in &pending {
            // Use BEGIN IMMEDIATE to acquire a write lock upfront, preventing
            // TOCTOU races when two processes start migrations concurrently.
            let mut conn = global_db.acquire().await?;
            sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

            // Re-check inside the write-locked transaction: another process
            // may have applied this version between our earlier SELECT and now.
            let already_applied: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM _rust_migrations WHERE version = ?)",
            )
            .bind(migration.version() as i64)
            .fetch_one(&mut *conn)
            .await?;

            if already_applied {
                sqlx::query("ROLLBACK").execute(&mut *conn).await?;
                continue;
            }

            let result = async {
                migration.run_global(&mut conn).await?;

                sqlx::query("INSERT INTO _rust_migrations (version, description) VALUES (?, ?)")
                    .bind(migration.version() as i64)
                    .bind(migration.description())
                    .execute(&mut *conn)
                    .await?;

                Ok::<(), DatabaseError>(())
            }
            .await;

            match result {
                Ok(()) => {
                    sqlx::query("COMMIT").execute(&mut *conn).await?;
                }
                Err(e) => {
                    let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                    return Err(e);
                }
            }
            count += 1;

            tracing::debug!(
                target: "whitenoise::database::rust_migrations",
                "Applied global migration v{}: {}",
                migration.version(),
                migration.description()
            );
        }

        tracing::info!(
            target: "whitenoise::database::rust_migrations",
            "{count} global Rust migration(s) applied in {}ms",
            start.elapsed().as_millis()
        );

        Ok(())
    }
}

pub struct LocalMigrationRunner {
    migrations: Vec<Box<dyn LocalMigration>>,
}

impl LocalMigrationRunner {
    /// Create a runner with the given migrations.
    ///
    /// # Panics
    ///
    /// Panics if any two migrations share the same version number.
    pub fn new(migrations: Vec<Box<dyn LocalMigration>>) -> Self {
        let mut seen = std::collections::HashSet::new();
        for m in &migrations {
            assert!(
                seen.insert(m.version()),
                "duplicate local migration version: {}",
                m.version()
            );
        }
        let mut runner = Self { migrations };
        runner.migrations.sort_by_key(|m| m.version());
        runner
    }

    /// Run all pending local migrations for a specific account.
    ///
    /// 1. Creates `_rust_migrations` tracking table on `local_db` if needed.
    /// 2. Applies pending Rust migrations in version order, each in its own
    ///    transaction.
    pub async fn run(
        &self,
        local_db: &SqlitePool,
        global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        let start = Instant::now();

        sqlx::query(CREATE_TRACKING_TABLE).execute(local_db).await?;

        let applied: Vec<(i64,)> = sqlx::query_as("SELECT version FROM _rust_migrations")
            .fetch_all(local_db)
            .await?;
        let applied: std::collections::HashSet<u32> =
            applied.iter().map(|(v,)| *v as u32).collect();

        let pending: Vec<&dyn LocalMigration> = self
            .migrations
            .iter()
            .filter(|m| !applied.contains(&m.version()))
            .map(|m| m.as_ref())
            .collect();

        if pending.is_empty() {
            return Ok(());
        }

        let mut count = 0u32;
        for migration in &pending {
            let mut conn = local_db.acquire().await?;
            sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

            let already_applied: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM _rust_migrations WHERE version = ?)",
            )
            .bind(migration.version() as i64)
            .fetch_one(&mut *conn)
            .await?;

            if already_applied {
                sqlx::query("ROLLBACK").execute(&mut *conn).await?;
                continue;
            }

            let result = async {
                migration
                    .run_local(&mut conn, global_db, account_pubkey)
                    .await?;

                sqlx::query("INSERT INTO _rust_migrations (version, description) VALUES (?, ?)")
                    .bind(migration.version() as i64)
                    .bind(migration.description())
                    .execute(&mut *conn)
                    .await?;

                Ok::<(), DatabaseError>(())
            }
            .await;

            match result {
                Ok(()) => {
                    sqlx::query("COMMIT").execute(&mut *conn).await?;
                }
                Err(e) => {
                    let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                    return Err(e);
                }
            }
            count += 1;

            tracing::debug!(
                target: "whitenoise::database::rust_migrations",
                "Applied local migration v{}: {} (account: {account_pubkey})",
                migration.version(),
                migration.description()
            );
        }

        tracing::info!(
            target: "whitenoise::database::rust_migrations",
            "{count} local Rust migration(s) applied in {}ms (account: {account_pubkey})",
            start.elapsed().as_millis()
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    async fn create_pool(dir: &TempDir, name: &str) -> SqlitePool {
        let path = dir.path().join(name);
        let url = format!("sqlite://{}?mode=rwc", path.display());
        SqlitePool::connect(&url).await.unwrap()
    }

    // -- GlobalMigrationRunner tests --

    struct FakeGlobal {
        ver: u32,
        desc: &'static str,
    }

    #[async_trait]
    impl GlobalMigration for FakeGlobal {
        fn version(&self) -> u32 {
            self.ver
        }
        fn description(&self) -> &'static str {
            self.desc
        }
        async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE TABLE IF NOT EXISTS fake_global_v{} (id INTEGER PRIMARY KEY)",
                self.ver
            )))
            .execute(&mut *tx)
            .await?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn global_empty_migration_list() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "empty.db").await;
        let runner = GlobalMigrationRunner::new(vec![]);

        runner.run(&pool).await.unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='_rust_migrations')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            exists,
            "tracking table should be created even with no migrations"
        );

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn global_single_migration() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "single.db").await;
        let runner = GlobalMigrationRunner::new(vec![Box::new(FakeGlobal {
            ver: 1,
            desc: "create test table",
        })]);

        runner.run(&pool).await.unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='fake_global_v1')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(exists);

        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn global_multiple_migrations_run_in_order() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "multi.db").await;

        // Register out of order to verify sorting.
        let runner = GlobalMigrationRunner::new(vec![
            Box::new(FakeGlobal {
                ver: 3,
                desc: "third",
            }),
            Box::new(FakeGlobal {
                ver: 1,
                desc: "first",
            }),
            Box::new(FakeGlobal {
                ver: 2,
                desc: "second",
            }),
        ]);

        runner.run(&pool).await.unwrap();

        for v in 1..=3 {
            let exists: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type='table' AND name='fake_global_v{v}')"
            )))
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(exists, "table for v{v} should exist");
        }

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    #[should_panic(expected = "duplicate global migration version: 1")]
    fn global_duplicate_version_panics() {
        GlobalMigrationRunner::new(vec![
            Box::new(FakeGlobal { ver: 1, desc: "a" }),
            Box::new(FakeGlobal { ver: 1, desc: "b" }),
        ]);
    }

    #[tokio::test]
    async fn global_idempotency() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "idempotent.db").await;
        let runner = GlobalMigrationRunner::new(vec![Box::new(FakeGlobal {
            ver: 1,
            desc: "test",
        })]);

        runner.run(&pool).await.unwrap();
        runner.run(&pool).await.unwrap();

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "migration should only be applied once");
    }

    struct FailingGlobal;

    #[async_trait]
    impl GlobalMigration for FailingGlobal {
        fn version(&self) -> u32 {
            2
        }
        fn description(&self) -> &'static str {
            "this will fail"
        }
        async fn run_global(&self, _tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
            Err(DatabaseError::Sqlx(sqlx::Error::Protocol(
                "intentional test failure".to_string(),
            )))
        }
    }

    #[tokio::test]
    async fn global_migration_failure_stops_subsequent() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "fail.db").await;
        let runner = GlobalMigrationRunner::new(vec![
            Box::new(FakeGlobal {
                ver: 1,
                desc: "succeeds",
            }),
            Box::new(FailingGlobal),
            Box::new(FakeGlobal {
                ver: 3,
                desc: "never reached",
            }),
        ]);

        let result = runner.run(&pool).await;
        assert!(result.is_err());

        // v1 should be applied (committed before v2 failed).
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1);

        // v2 and v3 should NOT be applied.
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version IN (2, 3)")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn global_failure_preserves_prior_on_rerun() {
        let dir = TempDir::new().unwrap();
        let pool = create_pool(&dir, "rerun.db").await;

        // First run: v1 succeeds, v2 fails.
        let runner = GlobalMigrationRunner::new(vec![
            Box::new(FakeGlobal {
                ver: 1,
                desc: "succeeds",
            }),
            Box::new(FailingGlobal),
        ]);
        let _ = runner.run(&pool).await;

        // Second run with v2 fixed: v1 should be skipped, v2 applied.
        let runner2 = GlobalMigrationRunner::new(vec![
            Box::new(FakeGlobal {
                ver: 1,
                desc: "succeeds",
            }),
            Box::new(FakeGlobal {
                ver: 2,
                desc: "now works",
            }),
        ]);
        runner2.run(&pool).await.unwrap();

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    // -- LocalMigrationRunner tests --

    struct FakeLocal {
        ver: u32,
        desc: &'static str,
    }

    #[async_trait]
    impl LocalMigration for FakeLocal {
        fn version(&self) -> u32 {
            self.ver
        }
        fn description(&self) -> &'static str {
            self.desc
        }
        async fn run_local(
            &self,
            tx: &mut SqliteConnection,
            _global_db: &SqlitePool,
            _account_pubkey: &str,
        ) -> Result<(), DatabaseError> {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE TABLE IF NOT EXISTS fake_local_v{} (id INTEGER PRIMARY KEY)",
                self.ver
            )))
            .execute(&mut *tx)
            .await?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn local_single_migration() {
        let dir = TempDir::new().unwrap();
        let local_pool = create_pool(&dir, "local.db").await;
        let global_pool = create_pool(&dir, "global.db").await;

        let runner = LocalMigrationRunner::new(vec![Box::new(FakeLocal {
            ver: 1,
            desc: "local test",
        })]);

        let pubkey = "aa".repeat(32);
        runner
            .run(&local_pool, &global_pool, &pubkey)
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='fake_local_v1')",
        )
        .fetch_one(&local_pool)
        .await
        .unwrap();
        assert!(exists);
    }

    #[tokio::test]
    async fn local_reads_from_global() {
        let dir = TempDir::new().unwrap();
        let local_pool = create_pool(&dir, "local_rg.db").await;
        let global_pool = create_pool(&dir, "global_rg.db").await;

        // Seed global DB with a value.
        sqlx::query("CREATE TABLE global_data (key TEXT PRIMARY KEY, val TEXT NOT NULL)")
            .execute(&global_pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO global_data (key, val) VALUES ('k1', 'hello')")
            .execute(&global_pool)
            .await
            .unwrap();

        struct CrossDbLocal;

        #[async_trait]
        impl LocalMigration for CrossDbLocal {
            fn version(&self) -> u32 {
                1
            }
            fn description(&self) -> &'static str {
                "cross-db read"
            }
            async fn run_local(
                &self,
                tx: &mut SqliteConnection,
                global_db: &SqlitePool,
                _account_pubkey: &str,
            ) -> Result<(), DatabaseError> {
                let (val,): (String,) =
                    sqlx::query_as("SELECT val FROM global_data WHERE key = 'k1'")
                        .fetch_one(global_db)
                        .await?;

                sqlx::query("CREATE TABLE local_copy (val TEXT NOT NULL)")
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO local_copy (val) VALUES (?)")
                    .bind(&val)
                    .execute(&mut *tx)
                    .await?;
                Ok(())
            }
        }

        let runner = LocalMigrationRunner::new(vec![Box::new(CrossDbLocal)]);
        let pubkey = "bb".repeat(32);
        runner
            .run(&local_pool, &global_pool, &pubkey)
            .await
            .unwrap();

        let (val,): (String,) = sqlx::query_as("SELECT val FROM local_copy")
            .fetch_one(&local_pool)
            .await
            .unwrap();
        assert_eq!(val, "hello");
    }

    #[test]
    #[should_panic(expected = "duplicate local migration version: 1")]
    fn local_duplicate_version_panics() {
        LocalMigrationRunner::new(vec![
            Box::new(FakeLocal { ver: 1, desc: "a" }),
            Box::new(FakeLocal { ver: 1, desc: "b" }),
        ]);
    }

    #[tokio::test]
    async fn local_multiple_migrations_run_in_order() {
        let dir = TempDir::new().unwrap();
        let local_pool = create_pool(&dir, "local_multi.db").await;
        let global_pool = create_pool(&dir, "global_multi.db").await;

        let runner = LocalMigrationRunner::new(vec![
            Box::new(FakeLocal {
                ver: 3,
                desc: "third",
            }),
            Box::new(FakeLocal {
                ver: 1,
                desc: "first",
            }),
            Box::new(FakeLocal {
                ver: 2,
                desc: "second",
            }),
        ]);

        let pubkey = "dd".repeat(32);
        runner
            .run(&local_pool, &global_pool, &pubkey)
            .await
            .unwrap();

        for v in 1..=3 {
            let exists: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type='table' AND name='fake_local_v{v}')"
            )))
            .fetch_one(&local_pool)
            .await
            .unwrap();
            assert!(exists, "table for v{v} should exist");
        }

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&local_pool)
            .await
            .unwrap();
        assert_eq!(count, 3);
    }

    struct FailingLocal;

    #[async_trait]
    impl LocalMigration for FailingLocal {
        fn version(&self) -> u32 {
            2
        }
        fn description(&self) -> &'static str {
            "this will fail"
        }
        async fn run_local(
            &self,
            _tx: &mut SqliteConnection,
            _global_db: &SqlitePool,
            _account_pubkey: &str,
        ) -> Result<(), DatabaseError> {
            Err(DatabaseError::Sqlx(sqlx::Error::Protocol(
                "intentional test failure".to_string(),
            )))
        }
    }

    #[tokio::test]
    async fn local_migration_failure_stops_subsequent() {
        let dir = TempDir::new().unwrap();
        let local_pool = create_pool(&dir, "local_fail.db").await;
        let global_pool = create_pool(&dir, "global_fail.db").await;

        let runner = LocalMigrationRunner::new(vec![
            Box::new(FakeLocal {
                ver: 1,
                desc: "succeeds",
            }),
            Box::new(FailingLocal),
            Box::new(FakeLocal {
                ver: 3,
                desc: "never reached",
            }),
        ]);

        let pubkey = "ee".repeat(32);
        let result = runner.run(&local_pool, &global_pool, &pubkey).await;
        assert!(result.is_err());

        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version = 1")
                .fetch_one(&local_pool)
                .await
                .unwrap();
        assert_eq!(count, 1);

        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version IN (2, 3)")
                .fetch_one(&local_pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn local_failure_preserves_prior_on_rerun() {
        let dir = TempDir::new().unwrap();
        let local_pool = create_pool(&dir, "local_rerun.db").await;
        let global_pool = create_pool(&dir, "global_rerun.db").await;

        let pubkey = "ff".repeat(32);

        // First run: v1 succeeds, v2 fails.
        let runner = LocalMigrationRunner::new(vec![
            Box::new(FakeLocal {
                ver: 1,
                desc: "succeeds",
            }),
            Box::new(FailingLocal),
        ]);
        let _ = runner.run(&local_pool, &global_pool, &pubkey).await;

        // Second run with v2 fixed: v1 should be skipped, v2 applied.
        let runner2 = LocalMigrationRunner::new(vec![
            Box::new(FakeLocal {
                ver: 1,
                desc: "succeeds",
            }),
            Box::new(FakeLocal {
                ver: 2,
                desc: "now works",
            }),
        ]);
        runner2
            .run(&local_pool, &global_pool, &pubkey)
            .await
            .unwrap();

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&local_pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn local_idempotency() {
        let dir = TempDir::new().unwrap();
        let local_pool = create_pool(&dir, "local_idem.db").await;
        let global_pool = create_pool(&dir, "global_idem.db").await;

        let runner = LocalMigrationRunner::new(vec![Box::new(FakeLocal {
            ver: 1,
            desc: "test",
        })]);

        let pubkey = "cc".repeat(32);
        runner
            .run(&local_pool, &global_pool, &pubkey)
            .await
            .unwrap();
        runner
            .run(&local_pool, &global_pool, &pubkey)
            .await
            .unwrap();

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&local_pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}

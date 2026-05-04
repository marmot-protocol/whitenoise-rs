use std::sync::LazyLock;
use std::time::Instant;

use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use super::DatabaseError;

mod m0001_bridge;
mod m0002_add_removed_at;
mod m0003_push_notifications;
mod m0004_add_muted_until;
mod m0005_group_push_tokens_member_identity;
mod m0006_add_self_removed;
mod m0007_search_position_index;
mod m0008_add_chat_cleared_at;
mod m0009_mute_list;
mod m0010_published_kp_kind_metadata;
mod m0011_delivery_status_account_scope;
mod m0012_bootstrap;
mod m0013_media_blob_reference_split;
mod m0014_drop_media_files;
mod m0015_published_events_pubkey_fk;
mod m0016_processed_events_pubkey_fk;
mod m0017_move_account_settings;
mod m0018_move_drafts;
mod m0019_move_published_key_packages;
mod m0020_move_published_events;
mod m0021_move_processed_events;
mod m0022_move_account_follows;
mod m0023_drop_shared_account_settings;
mod m0024_drop_shared_drafts;
mod m0025_drop_shared_published_key_packages;
mod m0026_drop_shared_published_events;
mod m0027_drop_shared_account_follows;
mod m0028_purge_account_processed_events;
mod m0029_move_media_references;
mod m0030_drop_shared_media_references;
mod m0032_move_push_registrations;
mod m0033_drop_shared_push_registrations;
mod m0034_move_group_push_tokens;
mod m0035_drop_shared_group_push_tokens;
mod m0036_move_accounts_groups;
mod m0037_drop_shared_accounts_groups;

/// All global migrations, in version order. Lifted from individual modules
/// so the test suite can build a globals-only `Migrator` for narrow tests.
pub fn all_global_migrations() -> Vec<Box<dyn GlobalMigration>> {
    vec![
        Box::new(m0001_bridge::Migration),
        Box::new(m0002_add_removed_at::Migration),
        Box::new(m0003_push_notifications::Migration),
        Box::new(m0004_add_muted_until::Migration),
        Box::new(m0005_group_push_tokens_member_identity::Migration),
        Box::new(m0006_add_self_removed::Migration),
        Box::new(m0007_search_position_index::Migration),
        Box::new(m0008_add_chat_cleared_at::Migration),
        Box::new(m0009_mute_list::Migration),
        Box::new(m0010_published_kp_kind_metadata::Migration),
        Box::new(m0011_delivery_status_account_scope::Migration),
        Box::new(m0013_media_blob_reference_split::Migration),
        Box::new(m0014_drop_media_files::Migration),
        Box::new(m0015_published_events_pubkey_fk::Migration),
        Box::new(m0016_processed_events_pubkey_fk::Migration),
        Box::new(m0023_drop_shared_account_settings::Migration),
        Box::new(m0024_drop_shared_drafts::Migration),
        Box::new(m0025_drop_shared_published_key_packages::Migration),
        Box::new(m0026_drop_shared_published_events::Migration),
        Box::new(m0027_drop_shared_account_follows::Migration),
        Box::new(m0028_purge_account_processed_events::Migration),
        Box::new(m0030_drop_shared_media_references::Migration),
        // Note: version 31 is occupied by m0012_bootstrap (BOOTSTRAP_VERSION = 31).
        // No m0031 file exists because the bootstrap migration claims that slot.
        Box::new(m0033_drop_shared_push_registrations::Migration),
        Box::new(m0035_drop_shared_group_push_tokens::Migration),
        Box::new(m0037_drop_shared_accounts_groups::Migration),
    ]
}

/// All local migrations, in version order.
pub fn all_local_migrations() -> Vec<Box<dyn LocalMigration>> {
    vec![
        Box::new(m0012_bootstrap::Migration),
        Box::new(m0017_move_account_settings::Migration),
        Box::new(m0018_move_drafts::Migration),
        Box::new(m0019_move_published_key_packages::Migration),
        Box::new(m0020_move_published_events::Migration),
        Box::new(m0021_move_processed_events::Migration),
        Box::new(m0022_move_account_follows::Migration),
        Box::new(m0029_move_media_references::Migration),
        Box::new(m0032_move_push_registrations::Migration),
        Box::new(m0034_move_group_push_tokens::Migration),
        Box::new(m0036_move_accounts_groups::Migration),
    ]
}

/// A migration that runs against the shared (cross-account) database.
///
/// Use for: app-wide schema changes, settings migrations, registries that
/// every account reads. Versions live in the *unified* migration timeline
/// shared with [`LocalMigration`] — see [`Migrator`].
#[async_trait]
pub trait GlobalMigration: Send + Sync {
    /// Unique version number across the *whole* migration timeline.
    ///
    /// Globals and locals share one ordered sequence. The [`Migrator`]
    /// asserts uniqueness across both kinds and executes migrations in
    /// version order so cross-scope dependencies (e.g. global X → local A →
    /// global Y) are honoured.
    fn version(&self) -> u32;

    /// Human-readable description for logging and the tracking table.
    fn description(&self) -> &'static str;

    /// Execute the migration inside an open transaction on the shared DB.
    ///
    /// The caller (runner) begins the transaction and passes it here.
    /// Return `Ok(())` on success. Any error rolls back the transaction and
    /// aborts startup.
    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError>;
}

/// A migration that runs against a per-account (local) database, with read
/// access to the shared database.
///
/// Use for: Phase 18 data extraction (shared → account), per-account data
/// transformations that need cross-DB reads. Versions live in the *unified*
/// migration timeline shared with [`GlobalMigration`] — see [`Migrator`].
#[async_trait]
pub trait LocalMigration: Send + Sync {
    /// Unique version number across the *whole* migration timeline.
    /// See [`GlobalMigration::version`].
    fn version(&self) -> u32;

    /// Human-readable description for logging and the tracking table.
    fn description(&self) -> &'static str;

    /// Whether this migration is meaningful only for accounts whose local
    /// DB has never had any local migration applied yet.
    ///
    /// When `true`, the runner **silently skips** this migration for
    /// non-fresh accounts — `run_local` is never called and no row is
    /// written to `_rust_migrations`. Use for: schema bootstraps that
    /// load a baseline snapshot, fresh-account-only initialization.
    /// Existing accounts reach the equivalent state through other
    /// migrations and must neither run nor record this one. The
    /// freshness gate is re-checked on every walk, so subsequent logins
    /// stay correctly skipped.
    ///
    /// "Fresh" is determined by snapshotting `_rust_migrations` at the
    /// start of the walk: an account is fresh iff none of its rows match a
    /// version belonging to a *local* migration in the timeline. Globals
    /// stamped on the same file (which can happen when a per-user file
    /// was opened by a generic `Database` constructor that runs globals
    /// on whichever pool it owns) do **not** count against freshness.
    fn for_new_accounts_only(&self) -> bool {
        false
    }

    /// Execute the migration inside an open transaction on the local DB.
    ///
    /// - `tx`: the per-account database transaction (writable).
    /// - `global_db`: the shared database pool (read-only within migration
    ///   context).
    /// - `account_pubkey`: hex pubkey of the account that owns this local DB.
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

/// One entry in the unified migration timeline.
///
/// Each entry is either a global or a local migration; the [`Migrator`]
/// stores them as a single version-sorted list so dispatch happens in one
/// pass.
enum TimelineEntry {
    Global(Box<dyn GlobalMigration>),
    Local(Box<dyn LocalMigration>),
}

impl TimelineEntry {
    fn version(&self) -> u32 {
        match self {
            Self::Global(m) => m.version(),
            Self::Local(m) => m.version(),
        }
    }

    fn kind_str(&self) -> &'static str {
        match self {
            Self::Global(_) => "global",
            Self::Local(_) => "local",
        }
    }
}

/// Walks a single ordered timeline of migrations across the shared DB and
/// any number of per-account DBs.
///
/// # Ordering guarantee
///
/// Globals and locals share a single version space. Migrations are executed
/// in strict version order regardless of scope. This is what lets a local
/// migration `A` at version N safely depend on schema added by a global `X`
/// at version N-1, even if a later global `Y` at version N+1 removes that
/// schema: walking `X → A → Y` in that order applies `A` while `X`'s schema
/// is still present.
///
/// # When to use which entry point
///
/// - [`Migrator::run`] with `account = Some(..)`: bring one account's DB up
///   to date and apply any pending globals to shared along the way. Use on
///   account-DB open.
/// - [`Migrator::run`] with `account = None`: shared-only mode. Applies
///   pending globals to shared and **skips locals**. Safe when no locals
///   exist or you know they have already been applied for every account on
///   disk; otherwise advancing globals past an unapplied local risks the
///   X-A-Y race for accounts that haven't logged in yet — prefer
///   [`Migrator::run_all`] in that case.
/// - [`Migrator::run_all`]: lockstep walk across the shared DB and every
///   supplied account DB. Strongest ordering guarantee — use at app
///   startup once per-account DB files are real (Phase 18b+).
pub struct Migrator {
    timeline: Vec<TimelineEntry>,
    /// Versions of every local migration in the timeline, used to decide
    /// whether an account DB is "fresh" with respect to the local timeline.
    /// An account is fresh iff none of its `_rust_migrations` rows match a
    /// version in this set — globals already stamped on the same file (e.g.
    /// when the per-user file was opened with [`Database::new`] which runs
    /// globals on its own pool) do **not** count against freshness.
    local_versions: std::collections::HashSet<u32>,
}

impl Migrator {
    /// Build a runner from the global and local migration registries.
    ///
    /// # Panics
    ///
    /// Panics if any two migrations share a version, regardless of scope —
    /// the unified timeline requires globally unique version numbers.
    pub fn new(
        globals: Vec<Box<dyn GlobalMigration>>,
        locals: Vec<Box<dyn LocalMigration>>,
    ) -> Self {
        let mut timeline: Vec<TimelineEntry> = Vec::with_capacity(globals.len() + locals.len());
        timeline.extend(globals.into_iter().map(TimelineEntry::Global));
        timeline.extend(locals.into_iter().map(TimelineEntry::Local));

        let mut seen: std::collections::HashMap<u32, &'static str> =
            std::collections::HashMap::new();
        for entry in &timeline {
            let v = entry.version();
            let kind = entry.kind_str();
            if let Some(prev) = seen.insert(v, kind) {
                panic!(
                    "duplicate migration version {v}: already registered as {prev}, \
                     now also as {kind}"
                );
            }
        }

        timeline.sort_by_key(TimelineEntry::version);

        let local_versions: std::collections::HashSet<u32> = timeline
            .iter()
            .filter_map(|e| match e {
                TimelineEntry::Local(m) => Some(m.version()),
                TimelineEntry::Global(_) => None,
            })
            .collect();

        Self {
            timeline,
            local_versions,
        }
    }

    /// Run pending migrations in unified version order.
    ///
    /// - Globals are applied to `shared` if not yet recorded there.
    /// - When `account` is `Some((local_pool, pubkey))`, locals are applied
    ///   to `local_pool` if not yet recorded there.
    /// - When `account` is `None`, locals are skipped (shared-only mode —
    ///   see the [`Migrator`] docs for the safety caveat).
    pub async fn run(
        &self,
        shared: &SqlitePool,
        account: Option<(&SqlitePool, &str)>,
    ) -> Result<(), DatabaseError> {
        let start = Instant::now();

        sqlx::query(CREATE_TRACKING_TABLE).execute(shared).await?;
        let applied_global = load_applied(shared).await?;

        let local_state = match account {
            Some((pool, pubkey)) => {
                sqlx::query(CREATE_TRACKING_TABLE).execute(pool).await?;
                let applied = load_applied(pool).await?;
                let was_fresh = applied.is_disjoint(&self.local_versions);
                Some((pool, pubkey, applied, was_fresh))
            }
            None => None,
        };

        let mut applied_count = 0u32;
        for entry in &self.timeline {
            match entry {
                TimelineEntry::Global(g) if !applied_global.contains(&g.version()) => {
                    apply_global(shared, g.as_ref()).await?;
                    applied_count += 1;
                }
                TimelineEntry::Local(l) => {
                    if let Some((pool, pubkey, applied, was_fresh)) = &local_state
                        && !applied.contains(&l.version())
                        && (!l.for_new_accounts_only() || *was_fresh)
                    {
                        apply_local(pool, shared, pubkey, l.as_ref()).await?;
                        applied_count += 1;
                    }
                }
                TimelineEntry::Global(_) => {}
            }
        }

        if applied_count > 0 {
            tracing::info!(
                target: "whitenoise::database::rust_migrations",
                "{applied_count} Rust migration(s) applied in {}ms",
                start.elapsed().as_millis()
            );
        }

        Ok(())
    }

    /// Eagerly run pending migrations in unified version order against the
    /// shared DB *and every supplied account DB in lockstep*.
    ///
    /// For each entry in the timeline:
    /// - If global: apply to `shared` if not already applied.
    /// - If local: apply to **every** account in `accounts` that hasn't
    ///   recorded it yet, before advancing to the next timeline entry.
    ///
    /// This is the strongest ordering guarantee available — every account on
    /// disk catches up before any subsequent global advances. Use at app
    /// startup once per-account DB files are real (Phase 18b+).
    pub async fn run_all(
        &self,
        shared: &SqlitePool,
        accounts: &[(SqlitePool, String)],
    ) -> Result<(), DatabaseError> {
        let start = Instant::now();

        sqlx::query(CREATE_TRACKING_TABLE).execute(shared).await?;
        let applied_global = load_applied(shared).await?;

        let mut applied_local: Vec<std::collections::HashSet<u32>> =
            Vec::with_capacity(accounts.len());
        let mut was_fresh: Vec<bool> = Vec::with_capacity(accounts.len());
        for (pool, _) in accounts {
            sqlx::query(CREATE_TRACKING_TABLE).execute(pool).await?;
            let applied = load_applied(pool).await?;
            was_fresh.push(applied.is_disjoint(&self.local_versions));
            applied_local.push(applied);
        }

        let mut applied_count = 0u32;
        for entry in &self.timeline {
            match entry {
                TimelineEntry::Global(g) if !applied_global.contains(&g.version()) => {
                    apply_global(shared, g.as_ref()).await?;
                    applied_count += 1;
                }
                TimelineEntry::Global(_) => {}
                TimelineEntry::Local(l) => {
                    for (((pool, pubkey), already), fresh) in accounts
                        .iter()
                        .zip(applied_local.iter())
                        .zip(was_fresh.iter())
                    {
                        if already.contains(&l.version()) {
                            continue;
                        }
                        if l.for_new_accounts_only() && !*fresh {
                            continue;
                        }
                        apply_local(pool, shared, pubkey, l.as_ref()).await?;
                        applied_count += 1;
                    }
                }
            }
        }

        if applied_count > 0 {
            tracing::info!(
                target: "whitenoise::database::rust_migrations",
                "{applied_count} Rust migration(s) applied in {}ms across {} account(s)",
                start.elapsed().as_millis(),
                accounts.len()
            );
        }

        Ok(())
    }
}

async fn load_applied(pool: &SqlitePool) -> Result<std::collections::HashSet<u32>, DatabaseError> {
    let rows: Vec<(i64,)> = sqlx::query_as("SELECT version FROM _rust_migrations")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|(v,)| *v as u32).collect())
}

async fn apply_global(
    shared: &SqlitePool,
    migration: &dyn GlobalMigration,
) -> Result<(), DatabaseError> {
    let mut conn = shared.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

    let already_applied: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM _rust_migrations WHERE version = ?)")
            .bind(migration.version() as i64)
            .fetch_one(&mut *conn)
            .await?;

    if already_applied {
        sqlx::query("ROLLBACK").execute(&mut *conn).await?;
        return Ok(());
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

    tracing::debug!(
        target: "whitenoise::database::rust_migrations",
        "Applied global migration v{}: {}",
        migration.version(),
        migration.description()
    );
    Ok(())
}

async fn apply_local(
    local: &SqlitePool,
    shared: &SqlitePool,
    account_pubkey: &str,
    migration: &dyn LocalMigration,
) -> Result<(), DatabaseError> {
    let mut conn = local.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

    let already_applied: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM _rust_migrations WHERE version = ?)")
            .bind(migration.version() as i64)
            .fetch_one(&mut *conn)
            .await?;

    if already_applied {
        sqlx::query("ROLLBACK").execute(&mut *conn).await?;
        return Ok(());
    }

    let result = async {
        migration
            .run_local(&mut conn, shared, account_pubkey)
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

    tracing::debug!(
        target: "whitenoise::database::rust_migrations",
        "Applied local migration v{}: {} (account: {account_pubkey})",
        migration.version(),
        migration.description()
    );
    Ok(())
}

/// The process-wide migrator built from every registered global and local
/// migration. Sorted into one unified timeline, with version uniqueness
/// asserted on first access.
pub static MIGRATOR: LazyLock<Migrator> =
    LazyLock::new(|| Migrator::new(all_global_migrations(), all_local_migrations()));

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    async fn create_pool(dir: &TempDir, name: &str) -> SqlitePool {
        let path = dir.path().join(name);
        let url = format!("sqlite://{}?mode=rwc", path.display());
        SqlitePool::connect(&url).await.unwrap()
    }

    // -- Test fixtures -------------------------------------------------------

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

    struct FailingGlobal {
        ver: u32,
    }

    #[async_trait]
    impl GlobalMigration for FailingGlobal {
        fn version(&self) -> u32 {
            self.ver
        }
        fn description(&self) -> &'static str {
            "failing global"
        }
        async fn run_global(&self, _tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
            Err(DatabaseError::Sqlx(sqlx::Error::Protocol(
                "intentional test failure".to_string(),
            )))
        }
    }

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

    struct FailingLocal {
        ver: u32,
    }

    #[async_trait]
    impl LocalMigration for FailingLocal {
        fn version(&self) -> u32 {
            self.ver
        }
        fn description(&self) -> &'static str {
            "failing local"
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

    /// Records each migration's version in the order it runs, so tests can
    /// assert on cross-scope ordering.
    struct OrderingGlobal {
        ver: u32,
        log: std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
    }

    #[async_trait]
    impl GlobalMigration for OrderingGlobal {
        fn version(&self) -> u32 {
            self.ver
        }
        fn description(&self) -> &'static str {
            "ordering probe (global)"
        }
        async fn run_global(&self, _tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
            self.log.lock().unwrap().push(self.ver);
            Ok(())
        }
    }

    struct OrderingLocal {
        ver: u32,
        log: std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
    }

    #[async_trait]
    impl LocalMigration for OrderingLocal {
        fn version(&self) -> u32 {
            self.ver
        }
        fn description(&self) -> &'static str {
            "ordering probe (local)"
        }
        async fn run_local(
            &self,
            _tx: &mut SqliteConnection,
            _global_db: &SqlitePool,
            _account_pubkey: &str,
        ) -> Result<(), DatabaseError> {
            self.log.lock().unwrap().push(self.ver);
            Ok(())
        }
    }

    // -- Construction --------------------------------------------------------

    #[tokio::test]
    async fn empty_migrator_creates_tracking_table() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "empty_shared.db").await;
        let migrator = Migrator::new(vec![], vec![]);

        migrator.run(&shared, None).await.unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='_rust_migrations')",
        )
        .fetch_one(&shared)
        .await
        .unwrap();
        assert!(exists);
    }

    #[test]
    #[should_panic(expected = "duplicate migration version 5")]
    fn duplicate_version_within_globals_panics() {
        Migrator::new(
            vec![
                Box::new(FakeGlobal { ver: 5, desc: "a" }),
                Box::new(FakeGlobal { ver: 5, desc: "b" }),
            ],
            vec![],
        );
    }

    #[test]
    #[should_panic(expected = "duplicate migration version 5")]
    fn duplicate_version_within_locals_panics() {
        Migrator::new(
            vec![],
            vec![
                Box::new(FakeLocal { ver: 5, desc: "a" }),
                Box::new(FakeLocal { ver: 5, desc: "b" }),
            ],
        );
    }

    #[test]
    #[should_panic(expected = "duplicate migration version 5")]
    fn duplicate_version_across_scopes_panics() {
        // The whole point of the unified timeline: a global and a local
        // cannot share a version.
        Migrator::new(
            vec![Box::new(FakeGlobal { ver: 5, desc: "g" })],
            vec![Box::new(FakeLocal { ver: 5, desc: "l" })],
        );
    }

    // -- Globals-only mode ---------------------------------------------------

    #[tokio::test]
    async fn run_none_applies_globals_only() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;

        let migrator = Migrator::new(
            vec![
                Box::new(FakeGlobal { ver: 1, desc: "g1" }),
                Box::new(FakeGlobal { ver: 3, desc: "g3" }),
            ],
            vec![Box::new(FakeLocal { ver: 2, desc: "l2" })],
        );

        migrator.run(&shared, None).await.unwrap();

        // Globals applied to shared.
        for v in [1, 3] {
            let exists: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type='table' AND name='fake_global_v{v}')"
            )))
            .fetch_one(&shared)
            .await
            .unwrap();
            assert!(exists, "global v{v} table should exist on shared");
        }

        // Local v2 NOT in the shared tracking table.
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version = 2")
                .fetch_one(&shared)
                .await
                .unwrap();
        assert_eq!(count, 0, "local should be skipped in shared-only mode");
    }

    #[tokio::test]
    async fn run_none_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;

        let migrator = Migrator::new(vec![Box::new(FakeGlobal { ver: 1, desc: "g" })], vec![]);
        migrator.run(&shared, None).await.unwrap();
        migrator.run(&shared, None).await.unwrap();

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&shared)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn run_none_global_failure_stops_subsequent() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;

        let migrator = Migrator::new(
            vec![
                Box::new(FakeGlobal { ver: 1, desc: "ok" }),
                Box::new(FailingGlobal { ver: 2 }),
                Box::new(FakeGlobal {
                    ver: 3,
                    desc: "never",
                }),
            ],
            vec![],
        );

        let result = migrator.run(&shared, None).await;
        assert!(result.is_err());

        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version = 1")
                .fetch_one(&shared)
                .await
                .unwrap();
        assert_eq!(count, 1);

        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version IN (2, 3)")
                .fetch_one(&shared)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }

    // -- Per-account mode ----------------------------------------------------

    #[tokio::test]
    async fn run_for_account_applies_locals_to_account() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let account = create_pool(&dir, "account.db").await;
        let pubkey = "aa".repeat(32);

        let migrator = Migrator::new(
            vec![],
            vec![Box::new(FakeLocal {
                ver: 1,
                desc: "local",
            })],
        );

        migrator
            .run(&shared, Some((&account, &pubkey)))
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='fake_local_v1')",
        )
        .fetch_one(&account)
        .await
        .unwrap();
        assert!(exists);

        // Local v1 recorded on account, not on shared.
        let (acc_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&account)
            .await
            .unwrap();
        assert_eq!(acc_count, 1);

        let (shared_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&shared)
            .await
            .unwrap();
        assert_eq!(shared_count, 0, "shared must not record local versions");
    }

    #[tokio::test]
    async fn run_for_account_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let account = create_pool(&dir, "account.db").await;
        let pubkey = "bb".repeat(32);

        let migrator = Migrator::new(
            vec![Box::new(FakeGlobal { ver: 1, desc: "g" })],
            vec![Box::new(FakeLocal { ver: 2, desc: "l" })],
        );

        migrator
            .run(&shared, Some((&account, &pubkey)))
            .await
            .unwrap();
        migrator
            .run(&shared, Some((&account, &pubkey)))
            .await
            .unwrap();

        let (g_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&shared)
            .await
            .unwrap();
        assert_eq!(g_count, 1);

        let (l_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&account)
            .await
            .unwrap();
        assert_eq!(l_count, 1);
    }

    #[tokio::test]
    async fn run_for_account_local_can_read_shared() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let account = create_pool(&dir, "account.db").await;
        let pubkey = "cc".repeat(32);

        sqlx::query("CREATE TABLE global_data (key TEXT PRIMARY KEY, val TEXT NOT NULL)")
            .execute(&shared)
            .await
            .unwrap();
        sqlx::query("INSERT INTO global_data (key, val) VALUES ('k1', 'hello')")
            .execute(&shared)
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

        let migrator = Migrator::new(vec![], vec![Box::new(CrossDbLocal)]);
        migrator
            .run(&shared, Some((&account, &pubkey)))
            .await
            .unwrap();

        let (val,): (String,) = sqlx::query_as("SELECT val FROM local_copy")
            .fetch_one(&account)
            .await
            .unwrap();
        assert_eq!(val, "hello");
    }

    #[tokio::test]
    async fn run_for_account_local_failure_stops_subsequent() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let account = create_pool(&dir, "account.db").await;
        let pubkey = "dd".repeat(32);

        let migrator = Migrator::new(
            vec![],
            vec![
                Box::new(FakeLocal { ver: 1, desc: "ok" }),
                Box::new(FailingLocal { ver: 2 }),
                Box::new(FakeLocal {
                    ver: 3,
                    desc: "never",
                }),
            ],
        );

        let result = migrator.run(&shared, Some((&account, &pubkey))).await;
        assert!(result.is_err());

        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version = 1")
                .fetch_one(&account)
                .await
                .unwrap();
        assert_eq!(count, 1);

        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version IN (2, 3)")
                .fetch_one(&account)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }

    // -- Cross-scope ordering (the whole point) ------------------------------

    #[tokio::test]
    async fn interleaved_migrations_run_in_unified_version_order() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let account = create_pool(&dir, "account.db").await;
        let pubkey = "ee".repeat(32);

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));

        // Register out of order to make sure the runner sorts.
        let migrator = Migrator::new(
            vec![
                Box::new(OrderingGlobal {
                    ver: 5,
                    log: log.clone(),
                }),
                Box::new(OrderingGlobal {
                    ver: 1,
                    log: log.clone(),
                }),
                Box::new(OrderingGlobal {
                    ver: 3,
                    log: log.clone(),
                }),
            ],
            vec![
                Box::new(OrderingLocal {
                    ver: 4,
                    log: log.clone(),
                }),
                Box::new(OrderingLocal {
                    ver: 2,
                    log: log.clone(),
                }),
            ],
        );

        migrator
            .run(&shared, Some((&account, &pubkey)))
            .await
            .unwrap();

        let observed = log.lock().unwrap().clone();
        assert_eq!(
            observed,
            vec![1, 2, 3, 4, 5],
            "globals and locals must run in unified version order, \
             not segregated by kind"
        );
    }

    #[tokio::test]
    async fn local_in_unified_timeline_runs_before_later_global() {
        // Models Javier's example: global X (v1, adds column) → local A (v2,
        // reads column) → global Y (v3, drops column). With segregated
        // sequences this would have been X→Y→A and A would fail. With the
        // unified timeline A runs before Y.
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let account = create_pool(&dir, "account.db").await;
        let pubkey = "ff".repeat(32);

        struct AddColumn;
        #[async_trait]
        impl GlobalMigration for AddColumn {
            fn version(&self) -> u32 {
                1
            }
            fn description(&self) -> &'static str {
                "add column"
            }
            async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
                sqlx::query("CREATE TABLE shared_t (id INTEGER PRIMARY KEY, vanishing TEXT)")
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO shared_t (id, vanishing) VALUES (1, 'still here')")
                    .execute(&mut *tx)
                    .await?;
                Ok(())
            }
        }

        struct CopyColumn;
        #[async_trait]
        impl LocalMigration for CopyColumn {
            fn version(&self) -> u32 {
                2
            }
            fn description(&self) -> &'static str {
                "copy column locally"
            }
            async fn run_local(
                &self,
                tx: &mut SqliteConnection,
                global_db: &SqlitePool,
                _account_pubkey: &str,
            ) -> Result<(), DatabaseError> {
                let (val,): (String,) =
                    sqlx::query_as("SELECT vanishing FROM shared_t WHERE id = 1")
                        .fetch_one(global_db)
                        .await?;
                sqlx::query("CREATE TABLE local_t (val TEXT NOT NULL)")
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO local_t (val) VALUES (?)")
                    .bind(val)
                    .execute(&mut *tx)
                    .await?;
                Ok(())
            }
        }

        struct DropColumn;
        #[async_trait]
        impl GlobalMigration for DropColumn {
            fn version(&self) -> u32 {
                3
            }
            fn description(&self) -> &'static str {
                "drop column"
            }
            async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
                sqlx::query("ALTER TABLE shared_t DROP COLUMN vanishing")
                    .execute(&mut *tx)
                    .await?;
                Ok(())
            }
        }

        let migrator = Migrator::new(
            vec![Box::new(AddColumn), Box::new(DropColumn)],
            vec![Box::new(CopyColumn)],
        );

        migrator
            .run(&shared, Some((&account, &pubkey)))
            .await
            .unwrap();

        // Local copied the value before global Y dropped the column.
        let (val,): (String,) = sqlx::query_as("SELECT val FROM local_t")
            .fetch_one(&account)
            .await
            .unwrap();
        assert_eq!(val, "still here");

        // Global Y did drop the column on shared.
        let cols: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(shared_t)")
                .fetch_all(&shared)
                .await
                .unwrap();
        let names: Vec<String> = cols.into_iter().map(|c| c.1).collect();
        assert!(
            !names.iter().any(|n| n == "vanishing"),
            "column should have been dropped after local copied it; columns: {names:?}"
        );
    }

    // -- run_all -------------------------------------------------------------

    #[tokio::test]
    async fn run_all_applies_local_to_every_account() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let acc_a = create_pool(&dir, "acc_a.db").await;
        let acc_b = create_pool(&dir, "acc_b.db").await;

        let migrator = Migrator::new(
            vec![Box::new(FakeGlobal { ver: 1, desc: "g" })],
            vec![Box::new(FakeLocal { ver: 2, desc: "l" })],
        );

        let accounts = vec![
            (acc_a.clone(), "aa".repeat(32)),
            (acc_b.clone(), "bb".repeat(32)),
        ];

        migrator.run_all(&shared, &accounts).await.unwrap();

        // Global recorded once on shared.
        let (shared_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations")
            .fetch_one(&shared)
            .await
            .unwrap();
        assert_eq!(shared_count, 1);

        // Local recorded on both accounts.
        for pool in [&acc_a, &acc_b] {
            let (count,): (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version = 2")
                    .fetch_one(pool)
                    .await
                    .unwrap();
            assert_eq!(count, 1);
        }
    }

    #[tokio::test]
    async fn run_all_walks_in_unified_order_across_accounts() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let acc_a = create_pool(&dir, "acc_a.db").await;
        let acc_b = create_pool(&dir, "acc_b.db").await;

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));

        let migrator = Migrator::new(
            vec![
                Box::new(OrderingGlobal {
                    ver: 1,
                    log: log.clone(),
                }),
                Box::new(OrderingGlobal {
                    ver: 3,
                    log: log.clone(),
                }),
            ],
            vec![Box::new(OrderingLocal {
                ver: 2,
                log: log.clone(),
            })],
        );

        let accounts = vec![(acc_a, "aa".repeat(32)), (acc_b, "bb".repeat(32))];
        migrator.run_all(&shared, &accounts).await.unwrap();

        // 1 (global), 2 (local for A), 2 (local for B), 3 (global).
        // Both local applications happen before the next global advances.
        assert_eq!(*log.lock().unwrap(), vec![1, 2, 2, 3]);
    }

    // -- new-accounts-only flag ---------------------------------------------

    /// A local migration that records its execution in a shared log and
    /// declares itself new-accounts-only.
    struct NewAccountsOnly {
        ver: u32,
        log: std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
    }

    #[async_trait]
    impl LocalMigration for NewAccountsOnly {
        fn version(&self) -> u32 {
            self.ver
        }
        fn description(&self) -> &'static str {
            "new-accounts-only probe"
        }
        fn for_new_accounts_only(&self) -> bool {
            true
        }
        async fn run_local(
            &self,
            _tx: &mut SqliteConnection,
            _global_db: &SqlitePool,
            _account_pubkey: &str,
        ) -> Result<(), DatabaseError> {
            self.log.lock().unwrap().push(self.ver);
            Ok(())
        }
    }

    #[tokio::test]
    async fn new_accounts_only_runs_for_fresh_account() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let account = create_pool(&dir, "account.db").await;
        let pubkey = "aa".repeat(32);

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
        let migrator = Migrator::new(
            vec![],
            vec![Box::new(NewAccountsOnly {
                ver: 12,
                log: log.clone(),
            })],
        );

        migrator
            .run(&shared, Some((&account, &pubkey)))
            .await
            .unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            vec![12],
            "should run on fresh account"
        );

        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version = 12")
                .fetch_one(&account)
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn new_accounts_only_skips_for_existing_account() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let account = create_pool(&dir, "account.db").await;
        let pubkey = "bb".repeat(32);

        // Pre-stamp a *local* version so the account is non-fresh w.r.t. the
        // local timeline. (Pre-stamping a global wouldn't mark it as
        // existing — globals live in a different timeline.)
        sqlx::query(CREATE_TRACKING_TABLE)
            .execute(&account)
            .await
            .unwrap();
        sqlx::query("INSERT INTO _rust_migrations (version, description) VALUES (15, 'prior')")
            .execute(&account)
            .await
            .unwrap();

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
        let migrator = Migrator::new(
            vec![],
            vec![
                Box::new(NewAccountsOnly {
                    ver: 12,
                    log: log.clone(),
                }),
                // A regular local at v=15 establishes the local-version
                // space the freshness check looks at.
                Box::new(FakeLocal {
                    ver: 15,
                    desc: "regular local",
                }),
            ],
        );

        migrator
            .run(&shared, Some((&account, &pubkey)))
            .await
            .unwrap();

        assert!(
            log.lock().unwrap().is_empty(),
            "must not run on a pre-existing account"
        );

        // No phantom row: skipping a for-new-only migration on a non-fresh
        // account writes nothing to `_rust_migrations`.
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version = 12")
                .fetch_one(&account)
                .await
                .unwrap();
        assert_eq!(
            count, 0,
            "skipping a for-new-only migration must not stamp a row"
        );
    }

    /// Phase 18b reality: when `AccountDatabase` opens its own per-user
    /// file via the existing `Database::new` (which runs globals on
    /// whichever pool it owns), the freshly-created file already has
    /// globals 1..N stamped in `_rust_migrations` *before* the local
    /// timeline ever runs. The bootstrap must still fire — globals don't
    /// count against local-timeline freshness.
    #[tokio::test]
    async fn new_accounts_only_runs_when_only_globals_are_pre_stamped() {
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let account = create_pool(&dir, "account.db").await;
        let pubkey = "bb".repeat(32);

        // Simulate a fresh per-user file that had `Database::new` run
        // globals on it before the account-aware migrator was invoked.
        sqlx::query(CREATE_TRACKING_TABLE)
            .execute(&account)
            .await
            .unwrap();
        for v in 1..=11 {
            sqlx::query(
                "INSERT INTO _rust_migrations (version, description) \
                 VALUES (?, 'pre-stamped global')",
            )
            .bind(v as i64)
            .execute(&account)
            .await
            .unwrap();
        }

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
        let migrator = Migrator::new(
            vec![],
            vec![Box::new(NewAccountsOnly {
                ver: 12,
                log: log.clone(),
            })],
        );

        migrator
            .run(&shared, Some((&account, &pubkey)))
            .await
            .unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            vec![12],
            "bootstrap must fire even when globals 1..11 are already stamped"
        );
    }

    #[tokio::test]
    async fn new_accounts_only_freshness_is_snapshot_at_walk_start() {
        // Two new-accounts-only migrations both run for a fresh account
        // even though the first stamps a row. Freshness must be a snapshot,
        // not re-derived each iteration.
        let dir = TempDir::new().unwrap();
        let shared = create_pool(&dir, "shared.db").await;
        let account = create_pool(&dir, "account.db").await;
        let pubkey = "cc".repeat(32);

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
        let migrator = Migrator::new(
            vec![],
            vec![
                Box::new(NewAccountsOnly {
                    ver: 12,
                    log: log.clone(),
                }),
                Box::new(NewAccountsOnly {
                    ver: 13,
                    log: log.clone(),
                }),
            ],
        );

        migrator
            .run(&shared, Some((&account, &pubkey)))
            .await
            .unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            vec![12, 13],
            "both new-accounts-only migrations must run when starting fresh"
        );
    }
}

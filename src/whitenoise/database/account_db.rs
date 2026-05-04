//! Per-account database wrapper — owns the account-scoped SQLite file.
//!
//! See `docs/session-projection-implementation-plan.md` for the full table
//! ownership audit and the per-phase migration plan.
//!
//! **Current status (Phase 18d shipped):** physically separate file per
//! account holding `account_settings`, `drafts`, `published_key_packages`,
//! `published_events`, `account_follows`, the account-scoped subset of
//! `processed_events`, plus `accounts_groups`, `push_registrations`, and
//! `group_push_tokens`. Phase 18e will move message projections.

use std::path::PathBuf;

use nostr_sdk::PublicKey;
use sqlx::SqlitePool;

use crate::whitenoise::database::{Database, DatabaseError, rust_migrations};

/// A connection to a per-account SQLite database.
///
/// Each account will have its own file, identified by the account's hex public
/// key. Account-scoped tables: `account_settings`, `account_follows`,
/// `accounts_groups`, `drafts`, `push_registrations`, `group_push_tokens`,
/// `published_key_packages`, `published_events`,
/// `processed_events` (account-level), `aggregated_messages`,
/// `message_delivery_status`, and `media_references`.
///
/// See `rearchitecture.md` Appendix B for the authoritative table ownership
/// map.
///
/// # Cross-scope FK audit
///
/// These FKs cross the account/shared boundary and require application-level
/// enforcement after the physical split. Phase 18c moved
/// `account_settings`, `drafts`, `published_key_packages`, `published_events`,
/// and `account_follows` out of shared. Phase 18d moved `accounts_groups`,
/// `push_registrations`, and `group_push_tokens`. Phase 18e will move
/// message projections.
///
/// ## Pubkey-keyed FKs (application-level enforcement only — Phase 18d)
///
/// - `accounts_groups.account_pubkey → accounts.pubkey` (moved in m0036)
/// - `media_files.account_pubkey → accounts.pubkey` (migration 0015)
/// - `push_registrations.account_pubkey → accounts.pubkey` (moved in m0032)
/// - `group_push_tokens.account_pubkey → accounts.pubkey` (moved in m0034)
///
/// ## Cross-scope data references (no schema FK, but logical dependency)
///
/// - `aggregated_messages.mls_group_id → group_information.mls_group_id`
///   (`group_information` is shared; `aggregated_messages` is account-scoped)
/// - `media_references.encrypted_file_hash → media_blobs.hash`
///   (`media_blobs` is shared; `media_references` is account-scoped)
///
/// ## Intra-account FKs (no cross-boundary issue)
///
/// - `message_delivery_status.aggregated_message_id → aggregated_messages.id`
///   (both account-scoped — will stay together)
///
/// # Current implementation
///
/// Thin newtype wrapper around [`Database`]. The split into separate files
/// happens in Phases 18b–18e.
#[derive(Clone, Debug)]
pub struct AccountDatabase {
    /// The account this database belongs to.
    pub(crate) account_pubkey: PublicKey,
    /// Underlying connection; will become its own `<pubkey>.db` pool post-split.
    pub(crate) inner: Database,
}

impl AccountDatabase {
    /// Open (or create) the account database for `account_pubkey` at `db_path`.
    ///
    /// Does **not** run migrations. Call [`run_account_migrations`](Self::run_account_migrations)
    /// with the real shared pool immediately after construction to apply
    /// pending globals (against shared) and locals (against this pool).
    pub async fn new(account_pubkey: PublicKey, db_path: PathBuf) -> Result<Self, DatabaseError> {
        let inner = Database::open_without_migrations(db_path).await?;
        Ok(Self {
            account_pubkey,
            inner,
        })
    }

    /// The public key that owns this database.
    pub fn account_pubkey(&self) -> &PublicKey {
        &self.account_pubkey
    }

    /// Path to the underlying SQLite file.
    pub fn path(&self) -> &PathBuf {
        &self.inner.path
    }

    /// Run pending local migrations against this account's DB.
    ///
    /// Must be called by the wiring whenever this `AccountDatabase` is
    /// opened (login or first-time create). Walks the unified migration
    /// timeline applying any pending globals to `shared` and any pending
    /// locals to this account's pool — including the new-account
    /// bootstrap on a freshly-created per-user file.
    ///
    /// Phase 18b+ entry point: per-user DB files become physically
    /// separate, so this is the natural place to fire the bootstrap.
    /// Currently (Phase 18a) account and shared resolve to the same SQLite
    /// file, but the freshness check on local-version space — not raw row
    /// presence — keeps the behaviour correct across the transition.
    pub async fn run_account_migrations(&self, shared: &SqlitePool) -> Result<(), DatabaseError> {
        let pubkey_hex = self.account_pubkey.to_hex();
        rust_migrations::MIGRATOR
            .run(shared, Some((&self.inner.pool, pubkey_hex.as_str())))
            .await?;
        tracing::info!(
            target: "whitenoise::database::account_db",
            account = %pubkey_hex,
            "Per-account migrations applied"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn test_pubkey() -> PublicKey {
        PublicKey::from_hex("000000000000000000000000000000000000000000000000000000000000cafe")
            .expect("valid test pubkey")
    }

    #[tokio::test]
    async fn test_account_database_opens_without_migrations() {
        let dir = TempDir::new().expect("temp dir");
        let pubkey = test_pubkey();
        let path = dir.path().join(format!("{}.db", pubkey.to_hex()));

        let db = AccountDatabase::new(pubkey, path.clone()).await;
        assert!(db.is_ok(), "expected Ok, got {db:?}");

        let db = db.unwrap();
        assert_eq!(db.path(), &path);
        assert_eq!(db.account_pubkey(), &test_pubkey());
    }

    #[tokio::test]
    async fn test_account_database_is_idempotent_across_opens() {
        let dir = TempDir::new().expect("temp dir");
        let pubkey = test_pubkey();
        let path = dir.path().join(format!("{}.db", pubkey.to_hex()));

        // First open.
        let _ = AccountDatabase::new(pubkey, path.clone())
            .await
            .expect("first open");

        // Second open — no migrations run, just file reopen.
        let db = AccountDatabase::new(test_pubkey(), path).await;
        assert!(db.is_ok(), "expected Ok on second open, got {db:?}");
    }

    /// Opening an account DB and then running `run_account_migrations`
    /// against a separate shared DB stamps the bootstrap (v=31) onto the
    /// account's `_rust_migrations` — proving the per-user-DB-creation
    /// hook fires the new-accounts-only timeline as designed.
    #[tokio::test]
    async fn test_run_account_migrations_fires_bootstrap_on_fresh_per_user_db() {
        let dir = TempDir::new().expect("temp dir");
        let shared_path = dir.path().join("shared.db");
        let account_path = dir.path().join(format!("{}.db", test_pubkey().to_hex()));

        // Open shared first so it has globals stamped against `shared.db`
        // (the realistic Phase 18b ordering).
        let shared = Database::new(shared_path).await.expect("open shared");

        // Open the per-user DB without migrations (the production path).
        // The bootstrap must fire when `run_account_migrations` is called.
        let account = AccountDatabase::new(test_pubkey(), account_path)
            .await
            .expect("open account");

        account
            .run_account_migrations(&shared.pool)
            .await
            .expect("run account migrations");

        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version = 31")
                .fetch_one(&account.inner.pool)
                .await
                .expect("query account tracking table");
        assert_eq!(
            count, 1,
            "bootstrap (v=31) must be recorded on the per-user DB"
        );

        // Description should be the bootstrap's regular one, not an
        // auto-stamp marker — proving `run_local` actually executed.
        let (desc,): (String,) =
            sqlx::query_as("SELECT description FROM _rust_migrations WHERE version = 31")
                .fetch_one(&account.inner.pool)
                .await
                .unwrap();
        assert!(
            !desc.contains("auto-stamped"),
            "bootstrap must run (not stamp) on a fresh per-user file; got desc: {desc}"
        );
    }

    /// Reopening an account DB must not drop tables that local migrations
    /// created (e.g. `accounts_groups`, `push_registrations`,
    /// `group_push_tokens`).
    ///
    /// Guards against `AccountDatabase::new` accidentally running global
    /// drop migrations (m0033, m0035, m0037) against the account pool.
    #[tokio::test]
    async fn test_account_tables_survive_reopen() {
        let dir = TempDir::new().expect("temp dir");
        let pubkey = test_pubkey();
        let pubkey_hex = pubkey.to_hex();
        let shared_path = dir.path().join("shared.db");
        let account_path = dir.path().join(format!("{pubkey_hex}.db"));

        // 1. Open shared + account without migrations.
        let shared = Database::open_without_migrations(shared_path)
            .await
            .expect("open shared");
        let account_pool = Database::open_without_migrations(account_path.clone())
            .await
            .expect("open account");

        // 2. Run lockstep migration (the boot path).
        let accounts = vec![(account_pool.pool.clone(), pubkey_hex.clone())];
        rust_migrations::MIGRATOR
            .run_all(&shared.pool, &accounts)
            .await
            .expect("run_all");

        // 3. Assert per-account tables exist.
        let account_tables = ["accounts_groups", "push_registrations", "group_push_tokens"];
        for table in &account_tables {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type='table' AND name=?)",
            )
            .bind(*table)
            .fetch_one(&account_pool.pool)
            .await
            .expect("check table exists");
            assert!(exists, "{table} must exist after run_all");
        }

        // 4. Drop the pool to simulate process restart.
        drop(account_pool);
        drop(accounts);

        // 5. Reopen via the production path.
        let account_db = AccountDatabase::new(pubkey, account_path)
            .await
            .expect("reopen account");
        account_db
            .run_account_migrations(&shared.pool)
            .await
            .expect("run_account_migrations after reopen");

        // 6. Assert tables still exist (the bug would have dropped them).
        for table in &account_tables {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type='table' AND name=?)",
            )
            .bind(*table)
            .fetch_one(&account_db.inner.pool)
            .await
            .expect("check table after reopen");
            assert!(
                exists,
                "{table} must survive reopen — global drops must not \
                 run against the account pool"
            );
        }
    }
}

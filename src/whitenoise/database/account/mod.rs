//! Per-account repository scaffold.
//!
//! This module exposes a small set of account-scoped repositories that wrap the
//! existing DB functions. Each repository stores an `account_pubkey` and an
//! `Arc<AccountDatabase>` (some also hold `Arc<Database>` for shared lookups),
//! and delegates to the underlying DB functions without re-exposing the pubkey
//! argument to callers.
//!
//! Additional repositories will be added here as their domains migrate to
//! session-scoped operations (see the session/projection implementation plan).

mod drafts;
mod follows;
mod group_push_tokens;
mod maintenance_tasks;
mod published_key_packages;
mod push_registrations;
mod settings;

use std::sync::Arc;

use nostr_sdk::PublicKey;

pub use self::drafts::DraftsRepo;
pub use self::follows::AccountFollowsRepo;
pub use self::group_push_tokens::GroupPushTokensRepo;
pub use self::maintenance_tasks::MaintenanceTasksRepo;
pub use self::published_key_packages::PublishedKeyPackagesRepo;
pub use self::push_registrations::PushRegistrationsRepo;
pub use self::settings::AccountSettingsRepo;

use crate::whitenoise::database::Database;
use crate::whitenoise::database::account_db::AccountDatabase;

/// All per-account repositories, bundled together for ergonomic access from
/// [`AccountSession`](crate::whitenoise::session::AccountSession).
#[derive(Clone, Debug)]
pub struct AccountRepositories {
    /// Draft message repository for this account.
    pub drafts: DraftsRepo,
    /// Account settings repository for this account.
    pub settings: AccountSettingsRepo,
    /// Follow relationship repository for this account.
    pub follows: AccountFollowsRepo,
    /// Published key package lifecycle tracking repository for this account.
    pub published_key_packages: PublishedKeyPackagesRepo,
    /// One-time maintenance task marker repository for this account.
    pub maintenance_tasks: MaintenanceTasksRepo,
    /// Push registration repository for this account.
    pub push_registrations: PushRegistrationsRepo,
    /// Cached group push tokens repository for this account.
    pub group_push_tokens: GroupPushTokensRepo,
}

impl AccountRepositories {
    /// Construct repositories for `account_pubkey` backed by `db` (shared) and
    /// `account_db` (per-account).
    pub(crate) async fn new(
        account_pubkey: PublicKey,
        db: Arc<Database>,
        account_db: Arc<AccountDatabase>,
    ) -> crate::whitenoise::error::Result<Self> {
        Ok(Self {
            drafts: DraftsRepo::new(account_db.clone()),
            settings: AccountSettingsRepo::new(account_db.clone()),
            follows: AccountFollowsRepo::new(account_db.clone(), db),
            push_registrations: PushRegistrationsRepo::new(account_pubkey, account_db.clone()),
            published_key_packages: PublishedKeyPackagesRepo::new(account_db.clone()),
            maintenance_tasks: MaintenanceTasksRepo::new(account_db.clone()),
            group_push_tokens: GroupPushTokensRepo::new(account_pubkey, account_db),
        })
    }
}

//! Per-account repository scaffold.
//!
//! This module exposes a small set of account-scoped repositories that wrap the
//! existing DB functions. Each repository stores an `account_pubkey` and an
//! `Arc<Database>`, and delegates to the underlying DB functions without
//! re-exposing the pubkey argument to callers.
//!
//! Additional repositories will be added here as their domains migrate to
//! session-scoped operations (see the session/projection implementation plan).

mod drafts;
mod follows;
mod settings;

use std::sync::Arc;

use nostr_sdk::PublicKey;

pub use self::drafts::DraftsRepo;
pub use self::follows::AccountFollowsRepo;
pub use self::settings::AccountSettingsRepo;

use crate::whitenoise::database::Database;

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
}

impl AccountRepositories {
    /// Construct repositories for `account_pubkey` backed by `db`.
    ///
    /// Returns an error if no account row exists for `account_pubkey` (required
    /// by [`AccountFollowsRepo`] to resolve the integer account id).
    pub(crate) async fn new(
        account_pubkey: PublicKey,
        db: Arc<Database>,
    ) -> crate::whitenoise::error::Result<Self> {
        Ok(Self {
            drafts: DraftsRepo::new(account_pubkey, db.clone()),
            settings: AccountSettingsRepo::new(account_pubkey, db.clone()),
            follows: AccountFollowsRepo::new(account_pubkey, db).await?,
        })
    }
}

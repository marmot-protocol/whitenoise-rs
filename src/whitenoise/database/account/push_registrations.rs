//! Per-account repository for push registrations.
//!
//! Wraps the existing [`PushRegistration`] DB functions so that callers do not
//! need to thread an `account_pubkey` argument through every call — the pubkey
//! is baked in at construction time.

use std::sync::Arc;

use nostr_sdk::{PublicKey, RelayUrl};

use crate::whitenoise::database::account_db::AccountDatabase;
use crate::whitenoise::error::Result;
use crate::whitenoise::push_notifications::{PushPlatform, PushRegistration};

/// Repository for push registrations scoped to a single account.
#[derive(Clone, Debug)]
pub struct PushRegistrationsRepo {
    account_pubkey: PublicKey,
    db: Arc<AccountDatabase>,
}

impl PushRegistrationsRepo {
    /// Construct a new [`PushRegistrationsRepo`] for `account_pubkey`.
    pub(crate) fn new(account_pubkey: PublicKey, db: Arc<AccountDatabase>) -> Self {
        Self { account_pubkey, db }
    }

    /// Return the push registration for this account, if one exists.
    pub async fn find(&self) -> Result<Option<PushRegistration>> {
        Ok(PushRegistration::find(&self.account_pubkey, &self.db.inner.pool).await?)
    }

    /// Create or replace the push registration for this account.
    pub async fn upsert(
        &self,
        platform: PushPlatform,
        raw_token: &str,
        server_pubkey: &PublicKey,
        relay_hint: Option<&RelayUrl>,
    ) -> Result<PushRegistration> {
        Ok(PushRegistration::upsert(
            &self.account_pubkey,
            platform,
            raw_token,
            server_pubkey,
            relay_hint,
            &self.db.inner.pool,
        )
        .await?)
    }

    /// Delete the push registration for this account. Returns `true` if a row
    /// was removed.
    pub async fn delete(&self) -> Result<bool> {
        Ok(PushRegistration::delete(&self.db.inner.pool).await?)
    }
}

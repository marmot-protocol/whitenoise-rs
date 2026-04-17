//! Account settings operations scoped to an [`AccountSession`].

use super::AccountSession;
use crate::whitenoise::account_settings::AccountSettings;
use crate::whitenoise::error::Result;

/// View over [`AccountSession`] for per-account settings operations.
///
/// Obtain via [`AccountSession::settings`].
pub struct SettingsOps<'a> {
    session: &'a AccountSession,
}

impl<'a> SettingsOps<'a> {
    pub(super) fn new(session: &'a AccountSession) -> Self {
        Self { session }
    }

    /// Return the settings for this account, creating a default row if none exists.
    pub async fn get(&self) -> Result<AccountSettings> {
        self.session.repos.settings.find_or_create().await
    }

    /// Set `notifications_enabled` and return the updated settings.
    ///
    /// **Note:** this only updates the database row. It does **not** perform
    /// push-token reconciliation. Callers that need the full side-effectful
    /// behaviour should use `Whitenoise::update_notifications_enabled` (which
    /// delegates here and then handles push-token sync separately).
    pub async fn update_notifications_enabled(&self, enabled: bool) -> Result<AccountSettings> {
        self.session
            .repos
            .settings
            .update_notifications_enabled(enabled)
            .await
    }
}

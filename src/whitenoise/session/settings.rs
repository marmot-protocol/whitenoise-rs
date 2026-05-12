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
    /// Also performs MIP-05 push-token reconciliation across this account's
    /// joined groups: enabling shares the local push token; disabling removes
    /// it. Reconciliation failures are logged but do not fail the call.
    pub async fn update_notifications_enabled(&self, enabled: bool) -> Result<AccountSettings> {
        let settings = self
            .session
            .repos
            .settings
            .update_notifications_enabled(enabled)
            .await?;

        let push = self.session.push();
        let reconciliation = if enabled {
            push.share_local_token_to_joined_groups().await
        } else {
            push.remove_local_token_from_joined_groups().await
        };

        if let Err(error) = reconciliation {
            tracing::warn!(
                target: "whitenoise::session::settings",
                account = %self.session.account_pubkey.to_hex(),
                enabled,
                error = %error,
                "Failed to reconcile shared push tokens after notification preference change"
            );
        }

        Ok(settings)
    }
}

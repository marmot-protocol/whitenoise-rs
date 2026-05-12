//! Social/follow operations scoped to an [`AccountSession`].

use nostr_sdk::PublicKey;

use super::AccountSession;
use crate::perf_instrument;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::users::User;

/// View over [`AccountSession`] for follow/unfollow operations.
///
/// Obtain via [`AccountSession::social`].
pub struct SocialOps<'a> {
    session: &'a AccountSession,
}

impl<'a> SocialOps<'a> {
    pub(super) fn new(session: &'a AccountSession) -> Self {
        Self { session }
    }

    /// Return all users this account follows.
    pub async fn follows(&self) -> Result<Vec<User>> {
        self.session.repos.follows.all().await
    }

    /// Return whether this account follows `target_pubkey`.
    pub async fn is_following(&self, target_pubkey: &PublicKey) -> Result<bool> {
        self.session.repos.follows.is_following(target_pubkey).await
    }

    /// Follow `target_pubkey`: resolve (or create) the user record, persist the
    /// follow row, and republish the account's follow list to relays. Every
    /// successful mutation requests a discovery rebuild so search and gossip
    /// see the updated graph.
    ///
    /// If the user was newly created, also kicks off background metadata
    /// resolution.
    #[perf_instrument("follows")]
    pub async fn follow_user(&self, target_pubkey: &PublicKey) -> Result<()> {
        let whitenoise = self.session.whitenoise()?;
        let (user, newly_created) =
            User::find_or_create_by_pubkey(target_pubkey, &self.session.shared.database).await?;

        if newly_created {
            whitenoise.start_background_user_resolution_if_unknown(user.pubkey);
        }

        self.session.repos.follows.add(&user).await?;
        self.session.shared.discovery_sync_worker.request_rebuild();
        self.background_publish_follow_list().await?;
        Ok(())
    }

    /// Unfollow `target_pubkey`: removes the local follow row, requests a
    /// discovery rebuild against the updated graph, and republishes the
    /// follow list. No-ops if the user is unknown locally.
    #[perf_instrument("follows")]
    pub async fn unfollow_user(&self, target_pubkey: &PublicKey) -> Result<()> {
        let whitenoise = self.session.whitenoise()?;
        let user = match whitenoise.find_user_by_pubkey(target_pubkey).await {
            Ok(user) => user,
            Err(WhitenoiseError::UserNotFound) => return Ok(()),
            Err(e) => return Err(e),
        };
        self.session.repos.follows.remove(&user).await?;
        self.session.shared.discovery_sync_worker.request_rebuild();
        self.background_publish_follow_list().await?;
        Ok(())
    }

    /// Spawns a task that publishes the current follow list to the account's
    /// NIP-65 relays. Delegates to the Whitenoise-level helper while it still
    /// owns the publish plumbing.
    async fn background_publish_follow_list(&self) -> Result<()> {
        let whitenoise = self.session.whitenoise()?;
        let account = crate::whitenoise::accounts::Account::find_by_pubkey(
            &self.session.account_pubkey,
            &self.session.shared.database,
        )
        .await?;
        whitenoise
            .background_publish_account_follow_list(&account)
            .await
    }
}

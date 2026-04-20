//! Social/follow operations scoped to an [`AccountSession`].

use nostr_sdk::PublicKey;

use super::AccountSession;
use crate::whitenoise::error::Result;
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

    /// Record that this account follows `user`.
    ///
    /// This is the low-level DB operation only. Callers that need the full
    /// side-effectful behaviour (user metadata resolution, Nostr follow-list
    /// publish) should use `Whitenoise::follow_user` instead.
    pub async fn add_follow(&self, user: &User) -> Result<()> {
        self.session.repos.follows.add(user).await
    }

    /// Remove the follow relationship between this account and `user`.
    ///
    /// This is the low-level DB operation only. Callers that need the full
    /// side-effectful behaviour (Nostr follow-list publish) should use
    /// `Whitenoise::unfollow_user` instead.
    pub async fn remove_follow(&self, user: &User) -> Result<()> {
        self.session.repos.follows.remove(user).await
    }
}

//! Group membership operations scoped to an [`AccountSession`].
//!
//! Provides [`MembershipOps`] (account-wide) and [`MembershipOpsForGroup`]
//! (narrowed to a single group) for managing the account↔group relationship:
//! acceptance/decline, read markers, pinning, archiving, muting, departure
//! tracking, DM peer lookup, and chat clearing.

use std::sync::Arc;

use chrono::Utc;
use mdk_core::prelude::GroupId;
use nostr_sdk::{EventId, PublicKey};

use super::AccountSession;
use crate::whitenoise::accounts_groups::{AccountGroup, MuteDuration};
use crate::whitenoise::aggregated_message::AggregatedMessage;
use crate::whitenoise::chat_list_streaming::ChatListUpdateTrigger;
use crate::whitenoise::database::Database;
use crate::whitenoise::error::{Result, WhitenoiseError};

/// Account-scoped membership view. Constructed via [`AccountSession::membership`].
pub struct MembershipOps<'a> {
    session: &'a AccountSession,
}

impl<'a> MembershipOps<'a> {
    pub(super) fn new(session: &'a AccountSession) -> Self {
        Self { session }
    }

    /// Narrow to a specific group.
    pub fn for_group(&self, group_id: &'a GroupId) -> MembershipOpsForGroup<'a> {
        MembershipOpsForGroup {
            session: self.session,
            group_id,
        }
    }

    fn pubkey(&self) -> &PublicKey {
        &self.session.account_pubkey
    }

    fn db(&self) -> &Arc<Database> {
        &self.session.shared.database
    }

    /// Return all visible (pending + accepted) account-group memberships.
    pub async fn visible_groups(&self) -> Result<Vec<AccountGroup>> {
        Ok(AccountGroup::find_visible_for_account(self.pubkey(), self.db()).await?)
    }

    /// Return all pending (awaiting user confirmation) account-group memberships.
    pub async fn pending_groups(&self) -> Result<Vec<AccountGroup>> {
        Ok(AccountGroup::find_pending_for_account(self.pubkey(), self.db()).await?)
    }

    /// Mark a message as read for this account.
    ///
    /// Looks up the message to find its group, then atomically advances the
    /// read marker only if the new message is newer than the current one.
    pub async fn mark_message_read(&self, message_id: &EventId) -> Result<AccountGroup> {
        let message = AggregatedMessage::find_by_message_id(message_id, self.db())
            .await?
            .ok_or(WhitenoiseError::MessageNotFound)?;

        let account_group = AccountGroup::find_by_account_and_group(
            self.pubkey(),
            &message.mls_group_id,
            self.db(),
        )
        .await?
        .ok_or(WhitenoiseError::GroupNotFound)?;

        let message_created_at_ms = message.created_at.timestamp_millis();
        if let Some(updated) = account_group
            .update_last_read_if_newer(message_id, message_created_at_ms, self.db())
            .await?
        {
            return Ok(updated);
        }

        // Update was skipped (message not newer), return current state.
        Ok(account_group)
    }

    /// Return the group ID of the latest DM group with `peer_pubkey`, if any.
    pub async fn dm_group_with_peer(&self, peer_pubkey: &PublicKey) -> Result<Option<GroupId>> {
        Ok(AccountGroup::find_dm_group_id_by_peer(self.pubkey(), peer_pubkey, self.db()).await?)
    }
}

/// Group-scoped membership view. Both account and group identity are baked in.
///
/// Constructed via [`MembershipOps::for_group`].
pub struct MembershipOpsForGroup<'a> {
    session: &'a AccountSession,
    group_id: &'a GroupId,
}

impl<'a> MembershipOpsForGroup<'a> {
    fn pubkey(&self) -> &PublicKey {
        &self.session.account_pubkey
    }

    fn db(&self) -> &Arc<Database> {
        &self.session.shared.database
    }

    /// Load the account-group row, returning `GroupNotFound` if absent.
    async fn require_account_group(&self) -> Result<AccountGroup> {
        AccountGroup::find_by_account_and_group(self.pubkey(), self.group_id, self.db())
            .await?
            .ok_or(WhitenoiseError::GroupNotFound)
    }

    /// Best-effort chat list stream notification routed through the session's
    /// own stream managers.
    async fn emit_chat_list_update(&self, trigger: ChatListUpdateTrigger) {
        self.session
            .chat_list()
            .emit_update(self.group_id, trigger)
            .await;
    }

    // ── Create / query ────────────────────────────────────────────

    /// Get or create the account-group membership.
    ///
    /// For DM groups, pass the peer's pubkey so it is persisted for efficient
    /// lookups. Returns `(AccountGroup, was_created)`.
    pub async fn get_or_create(
        &self,
        dm_peer_pubkey: Option<&PublicKey>,
    ) -> Result<(AccountGroup, bool)> {
        Ok(
            AccountGroup::find_or_create(self.pubkey(), self.group_id, dm_peer_pubkey, self.db())
                .await?,
        )
    }

    /// Return the last-read message ID for this account in this group.
    pub async fn last_read_message_id(&self) -> Result<Option<EventId>> {
        let account_group =
            AccountGroup::find_by_account_and_group(self.pubkey(), self.group_id, self.db())
                .await?;
        Ok(account_group.and_then(|ag| ag.last_read_message_id))
    }

    // ── Accept / decline ──────────────────────────────────────────

    /// Accept a group invite (DB-only, no push-token side effects).
    ///
    /// Callers that need push-token sharing should use
    /// `Whitenoise::accept_account_group` until push ops move to the session
    /// in Phase 13.
    pub async fn accept(&self) -> Result<AccountGroup> {
        let account_group = self.require_account_group().await?;
        Ok(account_group
            .update_user_confirmation(true, self.db())
            .await?)
    }

    /// Decline a group invite (DB-only, no push-token side effects).
    ///
    /// Callers that need push-token removal should use
    /// `Whitenoise::decline_account_group` until push ops move to the session
    /// in Phase 13.
    pub async fn decline(&self) -> Result<AccountGroup> {
        let account_group = self.require_account_group().await?;
        Ok(account_group
            .update_user_confirmation(false, self.db())
            .await?)
    }

    // ── Pin ───────────────────────────────────────────────────────

    /// Set or clear the pin order for this chat.
    ///
    /// `None` = unpin, `Some(n)` = pin at order n (lower values first).
    pub async fn set_pin_order(&self, pin_order: Option<i64>) -> Result<AccountGroup> {
        let account_group = self.require_account_group().await?;
        Ok(account_group.update_pin_order(pin_order, self.db()).await?)
    }

    // ── Archive ───────────────────────────────────────────────────

    /// Archive this chat. Idempotent.
    pub async fn archive(&self) -> Result<AccountGroup> {
        let account_group = self.require_account_group().await?;
        if account_group.is_archived() {
            return Ok(account_group);
        }
        let updated = account_group
            .update_archived_at(Some(Utc::now()), self.db())
            .await?;
        self.emit_chat_list_update(ChatListUpdateTrigger::ChatArchiveChanged)
            .await;
        Ok(updated)
    }

    /// Unarchive this chat. Idempotent.
    pub async fn unarchive(&self) -> Result<AccountGroup> {
        let account_group = self.require_account_group().await?;
        if !account_group.is_archived() {
            return Ok(account_group);
        }
        let updated = account_group.update_archived_at(None, self.db()).await?;
        self.emit_chat_list_update(ChatListUpdateTrigger::ChatArchiveChanged)
            .await;
        Ok(updated)
    }

    // ── Mute ──────────────────────────────────────────────────────

    /// Mute this chat for the specified duration.
    ///
    /// Re-muting overwrites the previous expiry. Returns
    /// [`WhitenoiseError::InvalidInput`] if the resolved expiry is not in
    /// the future.
    pub async fn mute(&self, duration: MuteDuration) -> Result<AccountGroup> {
        let until = duration.to_expiry();
        if until <= Utc::now() {
            return Err(WhitenoiseError::InvalidInput(
                "mute_chat: `until` must be in the future".to_string(),
            ));
        }
        let account_group = self.require_account_group().await?;
        let updated = account_group
            .update_muted_until(Some(until), self.db())
            .await?;
        self.emit_chat_list_update(ChatListUpdateTrigger::ChatMuteChanged)
            .await;
        Ok(updated)
    }

    /// Unmute this chat. Always clears `muted_until` and emits, even if the
    /// mute already expired.
    pub async fn unmute(&self) -> Result<AccountGroup> {
        let account_group = self.require_account_group().await?;
        let updated = account_group.update_muted_until(None, self.db()).await?;
        self.emit_chat_list_update(ChatListUpdateTrigger::ChatMuteChanged)
            .await;
        Ok(updated)
    }

    // ── Departure ─────────────────────────────────────────────────

    /// Mark this group as voluntarily departed.
    ///
    /// Idempotent. Emits [`ChatListUpdateTrigger::LeftGroup`] when the row
    /// changes.
    pub(crate) async fn mark_as_left(&self) -> Result<AccountGroup> {
        let account_group = self.require_account_group().await?;
        let Some(updated) = account_group.mark_left_atomic(self.db()).await? else {
            // Already departed — reload current state.
            return self.require_account_group().await;
        };
        self.emit_chat_list_update(ChatListUpdateTrigger::LeftGroup)
            .await;
        Ok(updated)
    }

    /// Mark this group as removed by an admin.
    ///
    /// Idempotent. Emits [`ChatListUpdateTrigger::RemovedFromGroup`] when
    /// the row changes.
    pub(crate) async fn mark_as_removed(&self) -> Result<AccountGroup> {
        let account_group = self.require_account_group().await?;
        let Some(updated) = account_group.mark_removed_atomic(self.db()).await? else {
            // Another task won the atomic update — reload persisted state.
            return self.require_account_group().await;
        };
        self.emit_chat_list_update(ChatListUpdateTrigger::RemovedFromGroup)
            .await;
        Ok(updated)
    }

    // ── Clear ─────────────────────────────────────────────────────

    /// Clear all messages from this account's view by setting a per-account
    /// timestamp floor.
    ///
    /// The group remains in the chat list with no visible messages. This is
    /// a local-only operation with no protocol-level side effects.
    pub async fn clear_chat(&self) -> Result<()> {
        let account_group = self.require_account_group().await?;

        account_group
            .clear_chat_state(Utc::now(), self.db())
            .await?;

        // Delete draft (best-effort).
        if let Err(e) = self.session.repos.drafts.delete(self.group_id).await {
            tracing::warn!(
                target: "whitenoise::membership",
                "Failed to delete draft during clear_chat: {e}"
            );
        }

        // Cleanup aggregated messages that all accounts have cleared (best-effort).
        if let Err(e) = self.db().try_cleanup_cleared_messages(self.group_id).await {
            tracing::warn!(
                target: "whitenoise::membership",
                "Failed to cleanup cleared messages: {e}"
            );
        }

        // Delete MDK message state (best-effort).
        if let Err(e) = self.session.mdk.delete_messages_for_group(self.group_id) {
            tracing::warn!(
                target: "whitenoise::membership",
                "Failed to delete MDK messages during clear_chat: {e}"
            );
        }

        self.emit_chat_list_update(ChatListUpdateTrigger::ChatCleared)
            .await;

        Ok(())
    }
}

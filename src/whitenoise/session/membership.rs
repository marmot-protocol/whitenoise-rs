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
use sqlx::SqlitePool;

use super::AccountSession;
use crate::whitenoise::accounts::Account;
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

    fn pool(&self) -> &SqlitePool {
        &self.session.account_db.inner.pool
    }

    /// Return all visible (pending + accepted) account-group memberships.
    pub async fn visible_groups(&self) -> Result<Vec<AccountGroup>> {
        Ok(AccountGroup::find_visible_for_account(self.pubkey(), self.pool()).await?)
    }

    /// Return all pending (awaiting user confirmation) account-group memberships.
    pub async fn pending_groups(&self) -> Result<Vec<AccountGroup>> {
        Ok(AccountGroup::find_pending_for_account(self.pubkey(), self.pool()).await?)
    }

    /// Mark a message as read for this account.
    ///
    /// Looks up the message to find its group, then advances the read marker
    /// only if the new message is newer than the current one. Uses CAS
    /// (compare-and-swap on `last_read_message_id`) to handle concurrent
    /// writers without cross-DB subqueries.
    pub async fn mark_message_read(&self, message_id: &EventId) -> Result<AccountGroup> {
        let message = AggregatedMessage::find_by_message_id(message_id, self.db())
            .await?
            .ok_or(WhitenoiseError::MessageNotFound)?;

        let message_created_at_ms = message.created_at.timestamp_millis();

        // CAS retry loop: re-read the marker if another writer changed it
        // between our read and our UPDATE (max 3 attempts).
        for _ in 0..3 {
            let account_group = AccountGroup::find_by_account_and_group(
                self.pubkey(),
                &message.mls_group_id,
                self.pool(),
            )
            .await?
            .ok_or(WhitenoiseError::GroupNotFound)?;

            // Phase 18e: switch to account pool once aggregated_messages moves.
            let current_marker_ts = match &account_group.last_read_message_id {
                Some(marker_id) => AggregatedMessage::created_at_ms_for_message(
                    marker_id,
                    &message.mls_group_id,
                    self.db(),
                )
                .await?
                .unwrap_or(0),
                None => 0,
            };

            match account_group
                .update_last_read_if_newer(
                    message_id,
                    message_created_at_ms,
                    account_group.last_read_message_id.as_ref(),
                    current_marker_ts,
                    self.pool(),
                )
                .await?
            {
                Some(updated) => return Ok(updated),
                None => {
                    // Either the message isn't newer (expected skip) or the
                    // CAS guard failed (concurrent writer). Re-read to
                    // distinguish: if the marker is now >= our message, the
                    // skip was correct and we're done.
                    let refreshed = AccountGroup::find_by_account_and_group(
                        self.pubkey(),
                        &message.mls_group_id,
                        self.pool(),
                    )
                    .await?
                    .ok_or(WhitenoiseError::GroupNotFound)?;

                    let refreshed_ts = match &refreshed.last_read_message_id {
                        Some(mid) => AggregatedMessage::created_at_ms_for_message(
                            mid,
                            &message.mls_group_id,
                            self.db(),
                        )
                        .await?
                        .unwrap_or(0),
                        None => 0,
                    };
                    if refreshed_ts >= message_created_at_ms {
                        // Another writer advanced past our message — done.
                        return Ok(refreshed);
                    }
                    // CAS miss: marker changed but is still behind our
                    // message. Retry with the fresh state.
                    continue;
                }
            }
        }

        // Exhausted retries — return current state. This is benign: the
        // marker will be corrected on the next mark_message_read call.
        let current = AccountGroup::find_by_account_and_group(
            self.pubkey(),
            &message.mls_group_id,
            self.pool(),
        )
        .await?
        .ok_or(WhitenoiseError::GroupNotFound)?;
        Ok(current)
    }

    /// Return the group ID of the latest DM group with `peer_pubkey`, if any.
    pub async fn dm_group_with_peer(&self, peer_pubkey: &PublicKey) -> Result<Option<GroupId>> {
        Ok(AccountGroup::find_dm_group_id_by_peer(peer_pubkey, self.pool()).await?)
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

    fn pool(&self) -> &SqlitePool {
        &self.session.account_db.inner.pool
    }

    /// Load the account-group row, returning `GroupNotFound` if absent.
    async fn require_account_group(&self) -> Result<AccountGroup> {
        AccountGroup::find_by_account_and_group(self.pubkey(), self.group_id, self.pool())
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
            AccountGroup::find_or_create(self.pubkey(), self.group_id, dm_peer_pubkey, self.pool())
                .await?,
        )
    }

    /// Return the last-read message ID for this account in this group.
    pub async fn last_read_message_id(&self) -> Result<Option<EventId>> {
        let account_group =
            AccountGroup::find_by_account_and_group(self.pubkey(), self.group_id, self.pool())
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
            .update_user_confirmation(true, self.pool())
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
            .update_user_confirmation(false, self.pool())
            .await?)
    }

    // ── Pin ───────────────────────────────────────────────────────

    /// Set or clear the pin order for this chat.
    ///
    /// `None` = unpin, `Some(n)` = pin at order n (lower values first).
    pub async fn set_pin_order(&self, pin_order: Option<i64>) -> Result<AccountGroup> {
        let account_group = self.require_account_group().await?;
        Ok(account_group
            .update_pin_order(pin_order, self.pool())
            .await?)
    }

    // ── Archive ───────────────────────────────────────────────────

    /// Archive this chat. Idempotent.
    pub async fn archive(&self) -> Result<AccountGroup> {
        let account_group = self.require_account_group().await?;
        if account_group.is_archived() {
            return Ok(account_group);
        }
        let updated = account_group
            .update_archived_at(Some(Utc::now()), self.pool())
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
        let updated = account_group.update_archived_at(None, self.pool()).await?;
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
            .update_muted_until(Some(until), self.pool())
            .await?;
        self.emit_chat_list_update(ChatListUpdateTrigger::ChatMuteChanged)
            .await;
        Ok(updated)
    }

    /// Unmute this chat. Always clears `muted_until` and emits, even if the
    /// mute already expired.
    pub async fn unmute(&self) -> Result<AccountGroup> {
        let account_group = self.require_account_group().await?;
        let updated = account_group.update_muted_until(None, self.pool()).await?;
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
        let Some(updated) = account_group.mark_left_atomic(self.pool()).await? else {
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
        let Some(updated) = account_group.mark_removed_atomic(self.pool()).await? else {
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
            .clear_chat_state(Utc::now(), self.pool())
            .await?;

        // Delete draft (best-effort).
        if let Err(e) = self.session.repos.drafts.delete(self.group_id).await {
            tracing::warn!(
                target: "whitenoise::session::membership",
                "Failed to delete draft during clear_chat: {e}"
            );
        }

        // Cleanup aggregated messages that all accounts have cleared (best-effort).
        let min_cleared_at = match self.resolve_min_cleared_at().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::session::membership",
                    "resolve_min_cleared_at failed; skipping cleanup: {e}"
                );
                None
            }
        };
        if let Err(e) = self
            .db()
            .try_cleanup_cleared_messages(self.group_id, min_cleared_at)
            .await
        {
            tracing::warn!(
                target: "whitenoise::session::membership",
                "Failed to cleanup cleared messages: {e}"
            );
        }

        // Delete MDK message state (best-effort).
        if let Err(e) = self.session.mdk.delete_messages_for_group(self.group_id) {
            tracing::warn!(
                target: "whitenoise::session::membership",
                "Failed to delete MDK messages during clear_chat: {e}"
            );
        }

        self.emit_chat_list_update(ChatListUpdateTrigger::ChatCleared)
            .await;

        Ok(())
    }

    /// Compute the minimum `chat_cleared_at` across all accounts for this
    /// group, returning `Ok(Some(millis))` only when **every** account has a
    /// non-null value. Returns `Ok(None)` if any account has not cleared or
    /// if not all accounts have active sessions (conservative — avoids
    /// premature message deletion). Returns `Err` on real DB failures so
    /// the caller can log the concrete error.
    async fn resolve_min_cleared_at(&self) -> Result<Option<i64>> {
        let wn = self
            .session
            .whitenoise
            .upgrade()
            .ok_or(WhitenoiseError::Initialization)?;
        let total_accounts = Account::count(&wn.shared.database).await?;
        let mut sessions_checked: i64 = 0;
        let mut min: Option<i64> = None;
        for session in wn.account_manager.sessions_iter() {
            sessions_checked += 1;
            match AccountGroup::chat_cleared_at_ms(
                &session.account_pubkey,
                self.group_id,
                &session.account_db.inner.pool,
            )
            .await
            {
                Ok(Some(ms)) => {
                    min = Some(min.map_or(ms, |prev: i64| prev.min(ms)));
                }
                Ok(None) => return Ok(None),
                Err(WhitenoiseError::GroupNotFound) => {
                    // This session doesn't have this group — skip it.
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        // If not all accounts have active sessions, we can't be sure every
        // account has cleared — return None to prevent premature deletion.
        if sessions_checked < total_accounts {
            tracing::warn!(
                target: "whitenoise::session::membership",
                checked = sessions_checked,
                total = total_accounts,
                "Not all accounts have active sessions; \
                 skipping cleared-message cleanup"
            );
            return Ok(None);
        }
        Ok(min)
    }
}

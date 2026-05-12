use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use mdk_core::prelude::GroupId;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::perf_instrument;
use sqlx::SqlitePool;

use crate::whitenoise::{
    Whitenoise, accounts::Account, chat_list_streaming::ChatListUpdateTrigger,
    database::media_files::MediaFile, error::WhitenoiseError, group_information::GroupInformation,
    push_notifications::GroupPushToken,
};

/// Represents the relationship between an account and an MLS group.
///
/// This struct tracks whether a user has accepted or declined a group invite.
/// When a welcome message is received, an AccountGroup is created with
/// `user_confirmation = None` (pending). The user can then accept or decline.
///
/// Confirmation states:
/// - `None` = pending (auto-joined but awaiting user decision)
/// - `Some(true)` = accepted
/// - `Some(false)` = declined (hidden from UI)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountGroup {
    pub id: Option<i64>,
    pub account_pubkey: PublicKey,
    pub mls_group_id: GroupId,
    pub user_confirmation: Option<bool>,
    pub welcomer_pubkey: Option<PublicKey>,
    pub last_read_message_id: Option<EventId>,
    /// Pin order for chat list sorting.
    /// - `None` = not pinned (appears after pinned chats)
    /// - `Some(n)` = pinned, lower values appear first
    pub pin_order: Option<i64>,
    /// For DM groups: the other participant's pubkey from this account's perspective.
    /// `None` for regular group chats.
    pub dm_peer_pubkey: Option<PublicKey>,
    /// When this chat was archived by this account.
    /// - `None` = not archived (active in chat list)
    /// - `Some(timestamp)` = archived at that time (hidden from main chat list)
    pub archived_at: Option<DateTime<Utc>>,
    /// When this account's membership in the group ended.
    /// - `None` = still an active member
    /// - `Some(timestamp)` = departed at that time (group is read-only, visible
    ///   in the chat list until the user explicitly archives or deletes it)
    pub removed_at: Option<DateTime<Utc>>,
    /// Whether the departure was voluntary (user chose to leave via SelfRemove).
    /// Only meaningful when `removed_at` is `Some`.
    /// - `false` = removed by an admin
    /// - `true` = user left voluntarily
    pub self_removed: bool,
    /// When this chat's mute expires.
    /// - `None` = not muted (notifications enabled)
    /// - `Some(timestamp)` = muted until that time; use `MUTE_FOREVER` for
    ///   "always muted until manually unmuted"
    pub muted_until: Option<DateTime<Utc>>,
    /// When this account last cleared the chat's messages.
    /// - `None` = not cleared (all messages visible)
    /// - `Some(timestamp)` = messages at or before this time are hidden
    pub chat_cleared_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Sentinel timestamp representing "muted forever" (9999-12-31T23:59:59Z).
/// Use this instead of crafting ad-hoc far-future timestamps.
pub(crate) const MUTE_FOREVER: DateTime<Utc> = {
    match DateTime::<Utc>::from_timestamp(253_402_300_799, 0) {
        Some(dt) => dt,
        None => unreachable!(),
    }
};

/// Mute duration for `AccountSession::membership().for_group().mute()`.
///
/// Use a preset variant for common durations or [`MuteDuration::Custom`] for an
/// arbitrary expiry timestamp. All variants resolve to a future [`DateTime<Utc>`]
/// via [`MuteDuration::to_expiry`]. Use [`MuteDuration::Forever`] to mute until
/// the user manually unmutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MuteDuration {
    #[serde(rename = "1h")]
    OneHour,
    #[serde(rename = "8h")]
    EightHours,
    #[serde(rename = "1d", alias = "24h", alias = "one_day")]
    OneDay,
    #[serde(rename = "1w", alias = "7d", alias = "one_week")]
    OneWeek,
    #[serde(rename = "forever")]
    Forever,
    /// Arbitrary expiry timestamp for callers that need a duration not covered
    /// by the preset variants. The timestamp must be in the future; `AccountSession::membership().for_group().mute()`
    /// will return [`WhitenoiseError::InvalidInput`] if it is not.
    #[serde(rename = "custom")]
    Custom(DateTime<Utc>),
}

impl MuteDuration {
    /// Converts this duration into an absolute [`DateTime<Utc>`] expiry timestamp.
    pub fn to_expiry(self) -> DateTime<Utc> {
        match self {
            Self::OneHour => Utc::now() + Duration::hours(1),
            Self::EightHours => Utc::now() + Duration::hours(8),
            Self::OneDay => Utc::now() + Duration::days(1),
            Self::OneWeek => Utc::now() + Duration::weeks(1),
            Self::Forever => MUTE_FOREVER,
            Self::Custom(until) => until,
        }
    }
}

impl FromStr for MuteDuration {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "1h" => Ok(Self::OneHour),
            "8h" => Ok(Self::EightHours),
            "1d" | "24h" | "one_day" => Ok(Self::OneDay),
            "1w" | "7d" | "one_week" => Ok(Self::OneWeek),
            "forever" => Ok(Self::Forever),
            _ => Err(format!(
                "invalid mute duration '{s}': expected 1h, 8h, 1d, 1w, forever, or a custom timestamp"
            )),
        }
    }
}

impl fmt::Display for MuteDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OneHour => write!(f, "1h"),
            Self::EightHours => write!(f, "8h"),
            Self::OneDay => write!(f, "1d"),
            Self::OneWeek => write!(f, "1w"),
            Self::Forever => write!(f, "forever"),
            Self::Custom(until) => write!(f, "custom({})", until.to_rfc3339()),
        }
    }
}

impl AccountGroup {
    /// Returns true if this group should be visible to the user.
    /// Visible means: pending (NULL), accepted (true), or removed (accepted but
    /// with `removed_at` set — group stays in the list read-only until archived).
    /// Declined groups (false) are always hidden.
    pub fn is_visible(&self) -> bool {
        self.user_confirmation != Some(false)
    }

    /// Returns true if this account is no longer a member of the group —
    /// either removed by an admin or voluntarily departed.
    pub fn is_removed(&self) -> bool {
        self.removed_at.is_some()
    }

    /// Returns true if this group is pending user confirmation.
    ///
    /// A removed group is never pending even if `user_confirmation` was `None` at the
    /// time of removal: an admin commit that removes the user supersedes the pending state.
    pub fn is_pending(&self) -> bool {
        self.user_confirmation.is_none() && self.removed_at.is_none()
    }

    /// Returns true if the user has accepted this group.
    pub fn is_accepted(&self) -> bool {
        self.user_confirmation == Some(true)
    }

    /// Returns true if the user has declined this group.
    pub fn is_declined(&self) -> bool {
        self.user_confirmation == Some(false)
    }

    /// Returns true if this chat is archived.
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }

    /// Returns true if this chat is currently muted (mute has not expired).
    pub fn is_muted(&self) -> bool {
        match self.muted_until {
            Some(until) => Utc::now() < until,
            None => false,
        }
    }

    /// Returns true if this chat is muted forever (until manually unmuted).
    pub fn is_muted_forever(&self) -> bool {
        self.muted_until >= Some(MUTE_FOREVER)
    }

    /// Returns true if a message with the given timestamp is visible — i.e., it
    /// was sent strictly after the chat was last cleared. When the chat has never
    /// been cleared, every message is visible.
    pub fn is_message_visible(&self, message_created_at: DateTime<Utc>) -> bool {
        self.chat_cleared_at
            .is_none_or(|cleared_at| message_created_at > cleared_at)
    }

    /// Look up the `chat_cleared_at` timestamp for an account/group pair and convert
    /// it to milliseconds since the Unix epoch, matching the format used by message
    /// query filters.
    ///
    /// Returns `Ok(None)` when the chat has never been cleared.
    /// Returns `Err(GroupNotFound)` when the account-group row does not exist.
    pub(crate) async fn chat_cleared_at_ms(
        pubkey: &PublicKey,
        group_id: &GroupId,
        pool: &SqlitePool,
    ) -> Result<Option<i64>, WhitenoiseError> {
        let account_group = Self::find_by_account_and_group(pubkey, group_id, pool)
            .await?
            .ok_or(WhitenoiseError::GroupNotFound)?;
        Ok(account_group
            .chat_cleared_at
            .map(|dt| dt.timestamp_millis()))
    }

    /// Creates or retrieves an AccountGroup for the given account and group.
    /// New records are created with user_confirmation = None (pending).
    ///
    /// For DM groups, pass the other participant's pubkey as `dm_peer_pubkey`
    /// so it can be persisted for efficient lookups.
    #[perf_instrument("account_groups")]
    pub async fn get_or_create(
        whitenoise: &Whitenoise,
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        dm_peer_pubkey: Option<&PublicKey>,
    ) -> Result<(Self, bool), WhitenoiseError> {
        let session = whitenoise.require_session(account_pubkey)?;
        let (account_group, was_created) = Self::find_or_create(
            account_pubkey,
            mls_group_id,
            dm_peer_pubkey,
            &session.account_db.inner.pool,
        )
        .await?;
        Ok((account_group, was_created))
    }

    /// Gets all visible AccountGroups for the given account.
    /// Visible means: pending or accepted (not declined).
    #[perf_instrument("account_groups")]
    pub async fn visible_for_account(
        whitenoise: &Whitenoise,
        account_pubkey: &PublicKey,
    ) -> Result<Vec<Self>, WhitenoiseError> {
        let session = whitenoise.require_session(account_pubkey)?;
        let groups =
            Self::find_visible_for_account(account_pubkey, &session.account_db.inner.pool).await?;
        Ok(groups)
    }

    /// Gets all pending AccountGroups for the given account.
    #[perf_instrument("account_groups")]
    pub async fn pending_for_account(
        whitenoise: &Whitenoise,
        account_pubkey: &PublicKey,
    ) -> Result<Vec<Self>, WhitenoiseError> {
        let session = whitenoise.require_session(account_pubkey)?;
        let groups =
            Self::find_pending_for_account(account_pubkey, &session.account_db.inner.pool).await?;
        Ok(groups)
    }

    /// Accepts this group invite by setting user_confirmation to true.
    #[perf_instrument("account_groups")]
    pub async fn accept(&self, whitenoise: &Whitenoise) -> Result<Self, WhitenoiseError> {
        let session = whitenoise.require_session(&self.account_pubkey)?;
        let updated = self
            .update_user_confirmation(true, &session.account_db.inner.pool)
            .await?;
        Ok(updated)
    }

    /// Finds the latest DM group between the given account and peer.
    ///
    /// Uses the persisted `dm_peer_pubkey` column for an efficient single-query
    /// lookup without requiring MLS/MDK calls. Returns the group ID of the most
    /// recently created visible DM group with this peer, or `None` if none exists.
    #[perf_instrument("account_groups")]
    pub async fn find_latest_dm_group_with_peer(
        whitenoise: &Whitenoise,
        account_pubkey: &PublicKey,
        peer_pubkey: &PublicKey,
    ) -> Result<Option<GroupId>, WhitenoiseError> {
        let session = whitenoise.require_session(account_pubkey)?;
        let group_id =
            Self::find_dm_group_id_by_peer(peer_pubkey, &session.account_db.inner.pool).await?;
        Ok(group_id)
    }

    /// Declines this group invite by setting user_confirmation to false.
    /// The group will be hidden from the UI but remains in MLS.
    #[perf_instrument("account_groups")]
    pub async fn decline(&self, whitenoise: &Whitenoise) -> Result<Self, WhitenoiseError> {
        let session = whitenoise.require_session(&self.account_pubkey)?;
        let updated = self
            .update_user_confirmation(false, &session.account_db.inner.pool)
            .await?;
        Ok(updated)
    }

    /// Archives this chat by setting archived_at to the current time.
    #[perf_instrument("account_groups")]
    pub async fn archive(&self, whitenoise: &Whitenoise) -> Result<Self, WhitenoiseError> {
        let session = whitenoise.require_session(&self.account_pubkey)?;
        let updated = self
            .update_archived_at(Some(Utc::now()), &session.account_db.inner.pool)
            .await?;
        Ok(updated)
    }

    /// Unarchives this chat by clearing archived_at.
    #[perf_instrument("account_groups")]
    pub async fn unarchive(&self, whitenoise: &Whitenoise) -> Result<Self, WhitenoiseError> {
        let session = whitenoise.require_session(&self.account_pubkey)?;
        let updated = self
            .update_archived_at(None, &session.account_db.inner.pool)
            .await?;
        Ok(updated)
    }

    /// Mutes this chat until the given time.
    #[perf_instrument("account_groups")]
    pub async fn mute(
        &self,
        until: DateTime<Utc>,
        whitenoise: &Whitenoise,
    ) -> Result<Self, WhitenoiseError> {
        let session = whitenoise.require_session(&self.account_pubkey)?;
        let updated = self
            .update_muted_until(Some(until), &session.account_db.inner.pool)
            .await?;
        Ok(updated)
    }

    /// Unmutes this chat by clearing muted_until.
    #[perf_instrument("account_groups")]
    pub async fn unmute(&self, whitenoise: &Whitenoise) -> Result<Self, WhitenoiseError> {
        let session = whitenoise.require_session(&self.account_pubkey)?;
        let updated = self
            .update_muted_until(None, &session.account_db.inner.pool)
            .await?;
        Ok(updated)
    }

    /// Delegates to [`mark_removed_atomic`](crate::whitenoise::database::accounts_groups::AccountGroup::mark_removed_atomic).
    /// Returns `Some` when the row was changed, `None` when already removed.
    #[perf_instrument("account_groups")]
    pub(crate) async fn mark_removed(
        &self,
        whitenoise: &Whitenoise,
    ) -> Result<Option<Self>, WhitenoiseError> {
        let session = whitenoise.require_session(&self.account_pubkey)?;
        self.mark_removed_atomic(&session.account_db.inner.pool)
            .await
            .map_err(WhitenoiseError::from)
    }

    /// Delegates to [`mark_left_atomic`](crate::whitenoise::database::accounts_groups::AccountGroup::mark_left_atomic).
    /// Returns `Some` when the row was changed, `None` when already departed.
    #[perf_instrument("account_groups")]
    pub(crate) async fn mark_left(
        &self,
        whitenoise: &Whitenoise,
    ) -> Result<Option<Self>, WhitenoiseError> {
        let session = whitenoise.require_session(&self.account_pubkey)?;
        self.mark_left_atomic(&session.account_db.inner.pool)
            .await
            .map_err(WhitenoiseError::from)
    }
}

impl Whitenoise {
    /// Deletes all local data for a group from this account's perspective.
    ///
    /// The chat disappears from the account's chat list entirely. This is a
    /// local-only operation -- no MLS proposals or Nostr events are published.
    ///
    /// If all accounts on this device have deleted the group, all shared data
    /// (group_information, aggregated_messages, etc.) is also physically removed.
    ///
    /// **Warning**: If the account has not left the MLS group first
    /// (`removed_at.is_none()`), they remain a member in the protocol -- other
    /// members continue encrypting messages for them, and their key packages
    /// remain published. The recommended flow is `leave_group` -> `delete_chat`.
    #[perf_instrument("account_groups")]
    pub async fn delete_chat(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<(), WhitenoiseError> {
        // 1. Look up session for account-DB access
        let session = self
            .account_manager
            .get_session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;

        // 2. Verify the AccountGroup exists
        let Some(_account_group) = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            group_id,
            &session.account_db.inner.pool,
        )
        .await?
        else {
            return Err(WhitenoiseError::GroupNotFound);
        };

        // 3. Pre-build the ChatListItem while data still exists
        //    (MDK state and DB data are needed to build the item)
        let chat_list_item = session.chat_list().build_item(group_id).await;

        // 4. Delete per-account accounts_groups row
        AccountGroup::delete_for_group(group_id, &session.account_db.inner.pool).await?;

        // Delete per-account aggregated_messages for this group. Order doesn't
        // matter relative to the steps below: there's no FK between
        // accounts_groups and aggregated_messages, and message_delivery_status
        // cascades via the intra-DB FK on aggregated_messages.
        crate::whitenoise::aggregated_message::AggregatedMessage::delete_by_group(
            group_id,
            &session.account_db.inner,
        )
        .await?;

        // 4a. Check if any other account still has this group.
        // We can only inspect active sessions, not every persisted account.
        // If inactive accounts exist, conservatively keep shared data — the
        // orphaned rows will be cleaned up on next login or full GC pass.
        let total_accounts = Account::count(&self.shared.database).await?;
        let mut sessions_checked: i64 = 0;
        let mut other_has_group = false;
        for other_session in self.account_manager.sessions_iter() {
            if other_session.account_pubkey == account.pubkey {
                continue;
            }
            sessions_checked += 1;
            let exists =
                AccountGroup::exists_for_group(group_id, &other_session.account_db.inner.pool)
                    .await?;
            if exists {
                other_has_group = true;
                break;
            }
        }
        // If we couldn't verify all other accounts, be conservative.
        if !other_has_group && sessions_checked + 1 < total_accounts {
            tracing::warn!(
                target: "whitenoise::accounts_groups",
                checked = sessions_checked,
                total = total_accounts,
                "Not all accounts have active sessions; \
                 skipping shared data deletion to be safe"
            );
            other_has_group = true;
        }

        // 4b. Delete shared group data if this was the last account
        self.shared
            .database
            .delete_shared_group_data(group_id, !other_has_group)
            .await?;

        // 4b. Delete per-account group_push_tokens for this group
        GroupPushToken::delete_by_group(group_id, &session.account_db.inner.pool).await?;

        // 4c. Delete per-account media_references for this group
        MediaFile::delete_references_for_group(&session.account_db.inner.pool, group_id).await?;

        // 4d. Delete per-account drafts for this group (best-effort)
        if let Err(e) = session.repos.drafts.delete(group_id).await {
            tracing::warn!(
                target: "whitenoise::accounts_groups",
                "Failed to delete draft during delete_chat: {e}"
            );
        }

        // 5. Delete MDK group state for this account (best-effort)
        match self.create_mdk_for_account(account.pubkey) {
            Ok(mdk) => {
                if let Err(e) = mdk.delete_group(group_id) {
                    tracing::warn!(
                        target: "whitenoise::accounts_groups",
                        "Failed to delete MDK group during delete_chat: {}",
                        e
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::accounts_groups",
                    "Failed to create MDK for delete_chat group cleanup: {}",
                    e
                );
            }
        }

        // 6. Clean up orphaned media files from disk
        let media_files = crate::whitenoise::media_files::MediaFiles::new(
            &self.shared.storage,
            &self.shared.database,
            &session.account_db.inner.pool,
        );
        if let Err(e) = media_files.cleanup_orphaned_files().await {
            tracing::warn!(
                target: "whitenoise::accounts_groups",
                "Failed to cleanup orphaned media files: {}",
                e
            );
        }

        // 6. Emit ChatDeleted with pre-built item
        if let Ok(Some(item)) = chat_list_item {
            let update = crate::whitenoise::chat_list_streaming::ChatListUpdate {
                trigger: ChatListUpdateTrigger::ChatDeleted,
                item,
            };
            self.shared
                .chat_list_stream_manager
                .emit(&account.pubkey, update.clone());
            self.shared
                .archived_chat_list_stream_manager
                .emit(&account.pubkey, update);
        }

        // 7. Refresh relay subscriptions
        self.background_refresh_account_group_subscriptions(account);

        Ok(())
    }

    /// Backfills the `dm_peer_pubkey` column for existing DM groups that are
    /// missing it.
    ///
    /// Uses a targeted SQL query to find only visible DM groups with NULL
    /// `dm_peer_pubkey`, then resolves the peer from MLS membership. Intended
    /// to be called once at startup.
    pub(crate) async fn backfill_dm_peer_pubkeys(&self) -> Result<(), WhitenoiseError> {
        for session in self.account_manager.sessions_iter() {
            let candidates =
                AccountGroup::find_groups_missing_peer(&session.account_db.inner.pool).await?;

            if candidates.is_empty() {
                continue;
            }

            let mdk = match self.create_mdk_for_account(session.account_pubkey) {
                Ok(mdk) => mdk,
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::accounts_groups",
                        "Failed to create MDK for account {}: {}",
                        session.account_pubkey, e
                    );
                    continue;
                }
            };

            for group_id in candidates {
                // Filter to DM groups using shared DB
                let is_dm = GroupInformation::is_dm(&group_id, &self.shared.database).await?;
                if !is_dm {
                    continue;
                }

                let members = match mdk.get_members(&group_id) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(
                            target: "whitenoise::accounts_groups",
                            group = %hex::encode(group_id.as_slice()),
                            "get_members failed during DM peer backfill: {e}"
                        );
                        continue;
                    }
                };

                let peer = members.iter().find(|pk| **pk != session.account_pubkey);
                if let Some(peer_pubkey) = peer
                    && let Err(e) = AccountGroup::update_dm_peer_pubkey(
                        &group_id,
                        peer_pubkey,
                        &session.account_db.inner.pool,
                    )
                    .await
                {
                    tracing::warn!(
                        target: "whitenoise::accounts_groups",
                        "Failed to backfill dm_peer_pubkey for group {}: {}",
                        hex::encode(group_id.as_slice()),
                        e
                    );
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::aggregated_message::AggregatedMessage;
    use crate::whitenoise::group_information::{GroupInformation, GroupType};
    use crate::whitenoise::group_state_streaming::GroupStateUpdate;
    use crate::whitenoise::test_utils::{create_mock_whitenoise, create_nostr_group_config_data};

    /// Helper to get the per-account pool from a Whitenoise instance + account pubkey.
    fn account_pool(whitenoise: &Whitenoise, pubkey: &PublicKey) -> SqlitePool {
        whitenoise
            .require_session(pubkey)
            .unwrap()
            .account_db
            .inner
            .pool
            .clone()
    }

    #[tokio::test]
    async fn test_account_group_visibility_methods() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[1; 32]);

        // Create a pending group
        let (pending_group, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        assert!(pending_group.is_visible());
        assert!(pending_group.is_pending());
        assert!(!pending_group.is_accepted());
        assert!(!pending_group.is_declined());

        // Accept it
        let accepted_group = pending_group.accept(&whitenoise).await.unwrap();

        assert!(accepted_group.is_visible());
        assert!(!accepted_group.is_pending());
        assert!(accepted_group.is_accepted());
        assert!(!accepted_group.is_declined());
    }

    #[tokio::test]
    async fn test_account_group_decline() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[2; 32]);

        let (pending_group, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        let declined_group = pending_group.decline(&whitenoise).await.unwrap();

        assert!(!declined_group.is_visible());
        assert!(!declined_group.is_pending());
        assert!(!declined_group.is_accepted());
        assert!(declined_group.is_declined());
    }

    #[tokio::test]
    async fn test_whitenoise_accept_account_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[3; 32]);

        // Create pending group
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Accept via Whitenoise method
        let accepted = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .accept()
            .await
            .unwrap();

        assert!(accepted.is_accepted());
    }

    #[tokio::test]
    async fn test_whitenoise_decline_account_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[4; 32]);

        // Create pending group
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Decline via Whitenoise method
        let declined = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .decline()
            .await
            .unwrap();

        assert!(declined.is_declined());
    }

    #[tokio::test]
    async fn test_get_visible_account_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let group_id1 = GroupId::from_slice(&[5; 32]); // pending
        let group_id2 = GroupId::from_slice(&[6; 32]); // accepted
        let group_id3 = GroupId::from_slice(&[7; 32]); // declined

        let (_, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id1)
            .get_or_create(None)
            .await
            .unwrap();

        let (ag2, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id2)
            .get_or_create(None)
            .await
            .unwrap();
        ag2.accept(&whitenoise).await.unwrap();

        let (ag3, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id3)
            .get_or_create(None)
            .await
            .unwrap();
        ag3.decline(&whitenoise).await.unwrap();

        let visible = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .membership()
            .visible_groups()
            .await
            .unwrap();

        assert_eq!(visible.len(), 2);
        let group_ids: Vec<_> = visible.iter().map(|ag| ag.mls_group_id.clone()).collect();
        assert!(group_ids.contains(&group_id1)); // pending is visible
        assert!(group_ids.contains(&group_id2)); // accepted is visible
        assert!(!group_ids.contains(&group_id3)); // declined is NOT visible
    }

    #[tokio::test]
    async fn test_get_pending_account_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let group_id1 = GroupId::from_slice(&[8; 32]); // pending
        let group_id2 = GroupId::from_slice(&[9; 32]); // accepted

        let (_, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id1)
            .get_or_create(None)
            .await
            .unwrap();

        let (ag2, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id2)
            .get_or_create(None)
            .await
            .unwrap();
        ag2.accept(&whitenoise).await.unwrap();

        let pending = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .membership()
            .pending_groups()
            .await
            .unwrap();

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].mls_group_id, group_id1);
    }

    #[tokio::test]
    async fn test_accept_nonexistent_group_returns_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[11; 32]);

        let result = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .accept()
            .await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WhitenoiseError::GroupNotFound
        ));
    }

    #[tokio::test]
    async fn test_decline_nonexistent_group_returns_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[12; 32]);

        let result = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .decline()
            .await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WhitenoiseError::GroupNotFound
        ));
    }

    #[tokio::test]
    async fn test_account_group_get_returns_none_for_nonexistent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[13; 32]);

        let result = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_account_group_get_returns_some_for_existing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[14; 32]);

        // Create an account group first
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Now get should return it
        let result = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap();

        assert!(result.is_some());
        let ag = result.unwrap();
        assert_eq!(ag.account_pubkey, account.pubkey);
        assert_eq!(ag.mls_group_id, group_id);
    }

    #[tokio::test]
    async fn test_get_or_create_returns_was_created_false_for_existing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[15; 32]);

        // First call creates
        let (_, was_created1) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();
        assert!(was_created1);

        // Second call finds existing
        let (_, was_created2) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();
        assert!(!was_created2);
    }

    #[tokio::test]
    async fn test_multiple_accounts_can_have_same_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account1 = whitenoise.create_identity().await.unwrap();
        let account2 = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[16; 32]);

        // Both accounts can have the same group
        let (ag1, _) = whitenoise
            .require_session(&account1.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();
        let (ag2, _) = whitenoise
            .require_session(&account2.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Both accounts have the group (in separate per-account DBs)
        assert_eq!(ag1.mls_group_id, ag2.mls_group_id);

        // Can have different confirmation states
        let accepted = ag1.accept(&whitenoise).await.unwrap();
        let declined = ag2.decline(&whitenoise).await.unwrap();

        assert!(accepted.is_accepted());
        assert!(declined.is_declined());
    }

    #[tokio::test]
    async fn test_pending_groups_empty_when_all_confirmed() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let group_id1 = GroupId::from_slice(&[17; 32]);
        let group_id2 = GroupId::from_slice(&[18; 32]);

        let (ag1, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id1)
            .get_or_create(None)
            .await
            .unwrap();
        let (ag2, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id2)
            .get_or_create(None)
            .await
            .unwrap();

        // Accept one, decline the other
        ag1.accept(&whitenoise).await.unwrap();
        ag2.decline(&whitenoise).await.unwrap();

        // No pending groups should remain
        let pending = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .membership()
            .pending_groups()
            .await
            .unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn test_is_visible_true_for_accepted() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[19; 32]);

        let (pending_group, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Pending should be visible
        assert!(pending_group.is_visible());

        // Accept and verify still visible
        let accepted_group = pending_group.accept(&whitenoise).await.unwrap();
        assert!(accepted_group.is_visible());
    }

    #[tokio::test]
    async fn test_is_visible_false_for_declined() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[20; 32]);

        let (pending_group, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        let declined_group = pending_group.decline(&whitenoise).await.unwrap();
        assert!(!declined_group.is_visible());
    }

    #[tokio::test]
    async fn test_same_account_multiple_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let group_id1 = GroupId::from_slice(&[21; 32]);
        let group_id2 = GroupId::from_slice(&[22; 32]);
        let group_id3 = GroupId::from_slice(&[23; 32]);

        // Create 3 groups for the same account
        let (ag1, c1) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id1)
            .get_or_create(None)
            .await
            .unwrap();
        let (ag2, c2) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id2)
            .get_or_create(None)
            .await
            .unwrap();
        let (ag3, c3) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id3)
            .get_or_create(None)
            .await
            .unwrap();

        assert!(c1 && c2 && c3);
        assert_ne!(ag1.id, ag2.id);
        assert_ne!(ag2.id, ag3.id);

        // All should be pending initially
        assert!(ag1.is_pending() && ag2.is_pending() && ag3.is_pending());
    }

    #[tokio::test]
    async fn test_mark_message_read_fails_for_nonexistent_message() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let fake_message_id = EventId::all_zeros();

        let result = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .mark_message_read(&fake_message_id)
            .await;

        assert!(matches!(result, Err(WhitenoiseError::MessageNotFound)));
    }

    #[tokio::test]
    async fn test_get_last_read_message_id_returns_none_when_not_set() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[99; 32]);

        // Create account group first
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        let result = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .last_read_message_id()
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_last_read_message_id_returns_none_for_nonexistent_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[98; 32]);

        // Don't create account group - it shouldn't exist
        let result = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .last_read_message_id()
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_mark_message_read_advances_forward() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[100; 32]);

        // Setup: create group_information (FK constraint) and account_group
        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Create two messages with different timestamps
        let now = Utc::now();
        let older_time = now - chrono::Duration::seconds(10);
        let newer_time = now;

        let older_msg_id = EventId::from_hex(&format!("{:0>64}", "aaa")).unwrap();
        let newer_msg_id = EventId::from_hex(&format!("{:0>64}", "bbb")).unwrap();

        let session = whitenoise.session(&account.pubkey).unwrap();
        AggregatedMessage::create_for_test(
            older_msg_id,
            group_id.clone(),
            account.pubkey,
            older_time,
            &session.account_db.inner,
        )
        .await
        .unwrap();

        AggregatedMessage::create_for_test(
            newer_msg_id,
            group_id.clone(),
            account.pubkey,
            newer_time,
            &session.account_db.inner,
        )
        .await
        .unwrap();

        // Mark older message as read first
        let ag = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .mark_message_read(&older_msg_id)
            .await
            .unwrap();
        assert_eq!(ag.last_read_message_id, Some(older_msg_id));

        // Mark newer message as read - should update
        let ag = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .mark_message_read(&newer_msg_id)
            .await
            .unwrap();
        assert_eq!(ag.last_read_message_id, Some(newer_msg_id));
    }

    #[tokio::test]
    async fn test_mark_message_read_does_not_regress() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[101; 32]);

        // Setup
        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Create two messages with different timestamps
        let now = Utc::now();
        let older_time = now - chrono::Duration::seconds(10);
        let newer_time = now;

        let older_msg_id = EventId::from_hex(&format!("{:0>64}", "ccc")).unwrap();
        let newer_msg_id = EventId::from_hex(&format!("{:0>64}", "ddd")).unwrap();

        let session = whitenoise.session(&account.pubkey).unwrap();
        AggregatedMessage::create_for_test(
            older_msg_id,
            group_id.clone(),
            account.pubkey,
            older_time,
            &session.account_db.inner,
        )
        .await
        .unwrap();

        AggregatedMessage::create_for_test(
            newer_msg_id,
            group_id.clone(),
            account.pubkey,
            newer_time,
            &session.account_db.inner,
        )
        .await
        .unwrap();

        // Mark NEWER message as read first
        let ag = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .mark_message_read(&newer_msg_id)
            .await
            .unwrap();
        assert_eq!(ag.last_read_message_id, Some(newer_msg_id));

        // Attempt to mark OLDER message as read - should be a no-op
        let ag = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .mark_message_read(&older_msg_id)
            .await
            .unwrap();
        // Should still be the newer message
        assert_eq!(ag.last_read_message_id, Some(newer_msg_id));
    }

    #[tokio::test]
    async fn test_mark_message_read_equal_timestamp_is_noop() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[102; 32]);

        // Setup
        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Create two messages with the SAME timestamp
        let same_time = Utc::now();

        let first_msg_id = EventId::from_hex(&format!("{:0>64}", "eee")).unwrap();
        let second_msg_id = EventId::from_hex(&format!("{:0>64}", "fff")).unwrap();

        let session = whitenoise.session(&account.pubkey).unwrap();
        AggregatedMessage::create_for_test(
            first_msg_id,
            group_id.clone(),
            account.pubkey,
            same_time,
            &session.account_db.inner,
        )
        .await
        .unwrap();

        AggregatedMessage::create_for_test(
            second_msg_id,
            group_id.clone(),
            account.pubkey,
            same_time,
            &session.account_db.inner,
        )
        .await
        .unwrap();

        // Mark first message as read
        let ag = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .mark_message_read(&first_msg_id)
            .await
            .unwrap();
        assert_eq!(ag.last_read_message_id, Some(first_msg_id));

        // Mark second message with same timestamp - should NOT update (equal is not newer)
        let ag = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .mark_message_read(&second_msg_id)
            .await
            .unwrap();
        assert_eq!(ag.last_read_message_id, Some(first_msg_id));
    }

    #[tokio::test]
    async fn test_set_chat_pin_order_pins_chat() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[50; 32]);

        // Create account group first
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Pin the chat
        let pinned = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .set_pin_order(Some(100))
            .await
            .unwrap();

        assert_eq!(pinned.pin_order, Some(100));
    }

    #[tokio::test]
    async fn test_set_chat_pin_order_unpins_chat() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[51; 32]);

        // Create and pin account group
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .set_pin_order(Some(100))
            .await
            .unwrap();

        // Unpin the chat
        let unpinned = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .set_pin_order(None)
            .await
            .unwrap();

        assert!(unpinned.pin_order.is_none());
    }

    #[tokio::test]
    async fn test_set_chat_pin_order_updates_existing_pin() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[52; 32]);

        // Create and pin account group
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .set_pin_order(Some(100))
            .await
            .unwrap();

        // Update pin order
        let updated = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .set_pin_order(Some(50))
            .await
            .unwrap();

        assert_eq!(updated.pin_order, Some(50));
    }

    #[tokio::test]
    async fn test_set_chat_pin_order_nonexistent_group_returns_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[53; 32]);

        // Don't create account group - it shouldn't exist
        let result = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .set_pin_order(Some(100))
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn test_get_dm_group_with_peer_returns_none_when_no_dms() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let other = whitenoise.create_identity().await.unwrap();

        let result = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .membership()
            .dm_group_with_peer(&other.pubkey)
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_dm_group_with_peer_finds_existing_dm() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let mut config = create_nostr_group_config_data(vec![creator.pubkey, member.pubkey]);
        config.name = String::new();
        let group = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member.pubkey], config, Some(GroupType::DirectMessage))
            .await
            .unwrap();

        let result = whitenoise
            .session(&creator.pubkey)
            .unwrap()
            .membership()
            .dm_group_with_peer(&member.pubkey)
            .await
            .unwrap();

        assert_eq!(result, Some(group.mls_group_id));
    }

    #[tokio::test]
    async fn test_get_dm_group_with_peer_ignores_regular_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        // Create a regular group (not a DM) with the same member
        let config = create_nostr_group_config_data(vec![creator.pubkey]);
        whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member.pubkey], config, Some(GroupType::Group))
            .await
            .unwrap();

        let result = whitenoise
            .session(&creator.pubkey)
            .unwrap()
            .membership()
            .dm_group_with_peer(&member.pubkey)
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_dm_group_with_peer_returns_latest_dm() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        // Create first DM
        let mut config1 = create_nostr_group_config_data(vec![creator.pubkey, member.pubkey]);
        config1.name = String::new();
        let _older_group = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member.pubkey], config1, Some(GroupType::DirectMessage))
            .await
            .unwrap();

        // Small delay to ensure different timestamps
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Create second DM with the same peer
        let mut config2 = create_nostr_group_config_data(vec![creator.pubkey, member.pubkey]);
        config2.name = String::new();
        let newer_group = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member.pubkey], config2, Some(GroupType::DirectMessage))
            .await
            .unwrap();

        let result = whitenoise
            .session(&creator.pubkey)
            .unwrap()
            .membership()
            .dm_group_with_peer(&member.pubkey)
            .await
            .unwrap();

        assert_eq!(result, Some(newer_group.mls_group_id));
    }

    #[tokio::test]
    async fn test_get_dm_group_with_peer_ignores_declined_dms() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let mut config = create_nostr_group_config_data(vec![creator.pubkey, member.pubkey]);
        config.name = String::new();
        let group = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member.pubkey], config, Some(GroupType::DirectMessage))
            .await
            .unwrap();

        // Decline the group
        whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .membership()
            .for_group(&group.mls_group_id)
            .decline()
            .await
            .unwrap();

        let result = whitenoise
            .session(&creator.pubkey)
            .unwrap()
            .membership()
            .dm_group_with_peer(&member.pubkey)
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_dm_group_with_peer_wrong_peer_returns_none() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();
        let stranger = whitenoise.create_identity().await.unwrap();

        let mut config = create_nostr_group_config_data(vec![creator.pubkey, member.pubkey]);
        config.name = String::new();
        whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member.pubkey], config, Some(GroupType::DirectMessage))
            .await
            .unwrap();

        // Search for a DM with a different user
        let result = whitenoise
            .session(&creator.pubkey)
            .unwrap()
            .membership()
            .dm_group_with_peer(&stranger.pubkey)
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_is_removed_false_by_default() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[100; 32]);

        let (ag, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        assert!(!ag.is_removed());
        assert!(ag.removed_at.is_none());
    }

    #[tokio::test]
    async fn test_mark_removed_sets_removed_at() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[101; 32]);

        let (ag, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        assert!(!ag.is_removed());

        let removed = ag
            .mark_removed(&whitenoise)
            .await
            .unwrap()
            .expect("first mark_removed must update the row");

        assert!(removed.is_removed());
        assert!(removed.removed_at.is_some());
        // A group removed before the user accepted must not surface as pending.
        assert!(!removed.is_pending());
    }

    #[tokio::test]
    async fn test_mark_as_removed_idempotent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[102; 32]);

        let (ag, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();
        let first = ag
            .mark_removed(&whitenoise)
            .await
            .unwrap()
            .expect("first mark_removed must update the row");

        // Second call via mark_as_removed must be a no-op (atomic WHERE removed_at IS NULL)
        let second = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mark_as_removed()
            .await
            .unwrap();

        assert_eq!(first.removed_at, second.removed_at);
    }

    #[tokio::test]
    async fn test_removed_group_is_still_visible() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[103; 32]);

        let (ag, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();
        let removed = ag
            .mark_removed(&whitenoise)
            .await
            .unwrap()
            .expect("first mark_removed must update the row");

        // Removed groups must still be visible — they stay in the chat list
        // until the user explicitly archives or deletes them
        assert!(removed.is_visible());
        assert!(removed.is_removed());
    }

    #[tokio::test]
    async fn test_is_muted_false_by_default() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[110; 32]);

        let (ag, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        assert!(!ag.is_muted());
        assert!(!ag.is_muted_forever());
        assert!(ag.muted_until.is_none());
    }

    #[tokio::test]
    async fn test_mute_chat_sets_muted_until() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[111; 32]);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        let muted = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mute(MuteDuration::OneHour)
            .await
            .unwrap();

        assert!(muted.is_muted());
        assert!(!muted.is_muted_forever());
        // muted_until should be approximately 1 hour from now
        let expected_min = Utc::now() + Duration::minutes(59);
        let expected_max = Utc::now() + Duration::minutes(61);
        let stored = muted.muted_until.unwrap();
        assert!(
            stored >= expected_min && stored <= expected_max,
            "muted_until should be ~1h from now"
        );
    }

    #[tokio::test]
    async fn test_mute_chat_forever() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[112; 32]);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        let muted = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mute(MuteDuration::Forever)
            .await
            .unwrap();

        assert!(muted.is_muted());
        assert!(muted.is_muted_forever());
        assert_eq!(muted.muted_until, Some(MUTE_FOREVER));
    }

    #[tokio::test]
    async fn test_unmute_chat_clears_muted_until() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[113; 32]);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Mute first
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mute(MuteDuration::Forever)
            .await
            .unwrap();

        // Unmute
        let unmuted = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .unmute()
            .await
            .unwrap();

        assert!(!unmuted.is_muted());
        assert!(!unmuted.is_muted_forever());
        assert!(unmuted.muted_until.is_none());
    }

    #[tokio::test]
    async fn test_unmute_chat_on_unmuted_chat_is_safe() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[114; 32]);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Unmute without ever muting — should succeed without error
        let result = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .unmute()
            .await
            .unwrap();

        assert!(!result.is_muted());
        assert!(result.muted_until.is_none());
    }

    #[tokio::test]
    async fn test_mute_chat_overwrites_previous_mute() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[115; 32]);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Mute for 1 hour
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mute(MuteDuration::OneHour)
            .await
            .unwrap();

        // Re-mute for 1 week — must overwrite, not be a no-op
        let remuted = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mute(MuteDuration::OneWeek)
            .await
            .unwrap();

        // muted_until should be approximately 1 week from now
        let expected_min = Utc::now() + Duration::days(6);
        let expected_max = Utc::now() + Duration::days(8);
        let stored = remuted.muted_until.unwrap();
        assert!(
            stored >= expected_min && stored <= expected_max,
            "re-mute must overwrite with ~1 week expiry"
        );
    }

    #[tokio::test]
    async fn test_is_muted_false_for_expired_mute() {
        let ag = AccountGroup {
            id: Some(1),
            account_pubkey: PublicKey::from_hex(
                "0000000000000000000000000000000000000000000000000000000000000001",
            )
            .unwrap(),
            mls_group_id: GroupId::from_slice(&[116; 32]),
            user_confirmation: Some(true),
            welcomer_pubkey: None,
            last_read_message_id: None,
            pin_order: None,
            dm_peer_pubkey: None,
            archived_at: None,
            removed_at: None,
            self_removed: false,
            // Expired 10 seconds ago
            muted_until: Some(Utc::now() - chrono::Duration::seconds(10)),
            chat_cleared_at: None,

            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        assert!(!ag.is_muted(), "expired mute should return false");
        assert!(!ag.is_muted_forever());
    }

    #[tokio::test]
    async fn test_mute_chat_nonexistent_group_returns_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[117; 32]);

        let result = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mute(MuteDuration::Forever)
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn test_unmute_chat_nonexistent_group_returns_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[118; 32]);

        let result = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .unmute()
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[test]
    fn test_mute_duration_to_expiry_all_variants() {
        let now = Utc::now();

        let expiry = MuteDuration::OneHour.to_expiry();
        assert!(expiry > now && expiry <= now + Duration::hours(1) + Duration::seconds(1));

        let expiry = MuteDuration::EightHours.to_expiry();
        assert!(expiry > now && expiry <= now + Duration::hours(8) + Duration::seconds(1));

        let expiry = MuteDuration::OneDay.to_expiry();
        assert!(expiry > now && expiry <= now + Duration::days(1) + Duration::seconds(1));

        let expiry = MuteDuration::OneWeek.to_expiry();
        assert!(expiry > now && expiry <= now + Duration::weeks(1) + Duration::seconds(1));

        assert_eq!(MuteDuration::Forever.to_expiry(), MUTE_FOREVER);

        let custom = now + Duration::hours(3);
        assert_eq!(MuteDuration::Custom(custom).to_expiry(), custom);
    }

    #[test]
    fn test_mute_duration_display() {
        assert_eq!(MuteDuration::OneHour.to_string(), "1h");
        assert_eq!(MuteDuration::EightHours.to_string(), "8h");
        assert_eq!(MuteDuration::OneDay.to_string(), "1d");
        assert_eq!(MuteDuration::OneWeek.to_string(), "1w");
        assert_eq!(MuteDuration::Forever.to_string(), "forever");

        let custom = Utc::now() + Duration::hours(3);
        let s = MuteDuration::Custom(custom).to_string();
        assert!(s.starts_with("custom(") && s.ends_with(')'));
    }

    #[test]
    fn test_mute_duration_from_str() {
        assert_eq!("1h".parse::<MuteDuration>().unwrap(), MuteDuration::OneHour);
        assert_eq!(
            "8h".parse::<MuteDuration>().unwrap(),
            MuteDuration::EightHours
        );
        assert_eq!("1d".parse::<MuteDuration>().unwrap(), MuteDuration::OneDay);
        assert_eq!("24h".parse::<MuteDuration>().unwrap(), MuteDuration::OneDay);
        assert_eq!(
            "one_day".parse::<MuteDuration>().unwrap(),
            MuteDuration::OneDay
        );
        assert_eq!("1w".parse::<MuteDuration>().unwrap(), MuteDuration::OneWeek);
        assert_eq!("7d".parse::<MuteDuration>().unwrap(), MuteDuration::OneWeek);
        assert_eq!(
            "one_week".parse::<MuteDuration>().unwrap(),
            MuteDuration::OneWeek
        );
        assert_eq!(
            "forever".parse::<MuteDuration>().unwrap(),
            MuteDuration::Forever
        );
        assert!("2h".parse::<MuteDuration>().is_err());
    }

    #[tokio::test]
    async fn test_mute_chat_custom_future_timestamp() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[131; 32]);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        let until = Utc::now() + chrono::Duration::days(3);
        let muted = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mute(MuteDuration::Custom(until))
            .await
            .unwrap();

        assert!(muted.is_muted());
        assert!(!muted.is_muted_forever());
        assert_eq!(
            muted.muted_until.map(|t| t.timestamp_millis()),
            Some(until.timestamp_millis())
        );
    }

    #[tokio::test]
    async fn test_mute_chat_rejects_past_custom_timestamp() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[130; 32]);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        let past = Utc::now() - chrono::Duration::hours(1);
        let result = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mute(MuteDuration::Custom(past))
            .await;

        assert!(
            matches!(result, Err(WhitenoiseError::InvalidInput(_))),
            "mute_chat should reject a Custom timestamp in the past"
        );
    }

    #[tokio::test]
    async fn test_muted_until_persists_through_save() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[119; 32]);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Mute the chat
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mute(MuteDuration::Forever)
            .await
            .unwrap();

        // Simulate a giftwrap re-processing: save() must NOT overwrite muted_until
        let re_save = AccountGroup {
            id: None,
            account_pubkey: account.pubkey,
            mls_group_id: group_id.clone(),
            user_confirmation: Some(true),
            welcomer_pubkey: None,
            last_read_message_id: None,
            pin_order: None,
            dm_peer_pubkey: None,
            archived_at: None,
            removed_at: None,
            self_removed: false,
            muted_until: None,
            chat_cleared_at: None,

            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        re_save
            .save(&account_pool(&whitenoise, &account.pubkey))
            .await
            .unwrap();

        // Fetch and confirm muted_until survived
        let found = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap()
        .unwrap();

        assert!(
            found.is_muted(),
            "save() must not overwrite muted_until — giftwrap re-processing would silently unmute chats"
        );
        assert!(found.is_muted_forever());
    }

    #[tokio::test]
    async fn test_clear_expired_mutes_clears_expired_rows() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id1 = GroupId::from_slice(&[120; 32]);
        let group_id2 = GroupId::from_slice(&[121; 32]);

        // Create two groups and set both to expired mutes via update_muted_until
        // (mute_chat rejects past timestamps, so we go through the DB layer directly)
        let (ag1, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id1)
            .get_or_create(None)
            .await
            .unwrap();
        let (ag2, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id2)
            .get_or_create(None)
            .await
            .unwrap();

        let past = Utc::now() - chrono::Duration::seconds(10);
        ag1.update_muted_until(Some(past), &account_pool(&whitenoise, &account.pubkey))
            .await
            .unwrap();
        ag2.update_muted_until(Some(past), &account_pool(&whitenoise, &account.pubkey))
            .await
            .unwrap();

        // Run cleanup
        let cleared = AccountGroup::clear_expired_mutes(
            &account.pubkey,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap();

        assert_eq!(cleared.len(), 2);

        // Verify DB rows are actually cleared
        let ag1 = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id1,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap()
        .unwrap();
        let ag2 = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id2,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap()
        .unwrap();

        assert!(ag1.muted_until.is_none());
        assert!(ag2.muted_until.is_none());
    }

    #[tokio::test]
    async fn test_clear_expired_mutes_preserves_active_mutes() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let expired_group = GroupId::from_slice(&[122; 32]);
        let forever_group = GroupId::from_slice(&[123; 32]);
        let future_group = GroupId::from_slice(&[124; 32]);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&expired_group)
            .get_or_create(None)
            .await
            .unwrap();
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&forever_group)
            .get_or_create(None)
            .await
            .unwrap();
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&future_group)
            .get_or_create(None)
            .await
            .unwrap();

        // One expired (set via DB layer), one forever, one future
        let past = Utc::now() - chrono::Duration::seconds(10);
        let expired_ag = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &expired_group,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap()
        .unwrap();
        expired_ag
            .update_muted_until(Some(past), &account_pool(&whitenoise, &account.pubkey))
            .await
            .unwrap();
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&forever_group)
            .mute(MuteDuration::Forever)
            .await
            .unwrap();
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&future_group)
            .mute(MuteDuration::OneHour)
            .await
            .unwrap();

        // Run cleanup — should only clear the expired one
        let cleared = AccountGroup::clear_expired_mutes(
            &account.pubkey,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap();

        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].mls_group_id, expired_group);

        // Active mutes must be untouched
        let forever_ag = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &forever_group,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap()
        .unwrap();
        let future_ag = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &future_group,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap()
        .unwrap();

        assert!(forever_ag.is_muted_forever());
        assert!(future_ag.is_muted());
    }

    #[tokio::test]
    async fn test_removed_at_persists_through_save() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[104; 32]);

        let (ag, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Mark as removed
        ag.mark_removed(&whitenoise).await.unwrap().unwrap();

        // Simulate a giftwrap re-processing: save() must NOT overwrite removed_at
        let re_save = AccountGroup {
            id: None,
            account_pubkey: account.pubkey,
            mls_group_id: group_id.clone(),
            user_confirmation: Some(true),
            welcomer_pubkey: None,
            last_read_message_id: None,
            pin_order: None,
            dm_peer_pubkey: None,
            archived_at: None,
            removed_at: None,
            self_removed: false,
            muted_until: None,
            chat_cleared_at: None,

            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        re_save
            .save(&account_pool(&whitenoise, &account.pubkey))
            .await
            .unwrap();

        // Fetch and confirm removed_at survived
        let found = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap()
        .unwrap();

        assert!(
            found.is_removed(),
            "save() must not overwrite removed_at — giftwrap re-processing would silently un-remove chats"
        );
    }

    #[tokio::test]
    async fn test_self_removed_false_by_default() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[110; 32]);

        let (ag, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        assert!(!ag.self_removed);
        assert!(!ag.is_removed());
    }

    #[tokio::test]
    async fn test_mark_as_left_sets_self_removed() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[111; 32]);

        let (ag, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        assert!(!ag.self_removed);

        let left = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mark_as_left()
            .await
            .unwrap();

        assert!(left.is_removed(), "removed_at must be set");
        assert!(left.self_removed, "self_removed must be true");
        assert!(!left.is_pending(), "must not be pending after leaving");
        assert!(left.is_visible(), "left groups stay visible until archived");
    }

    #[tokio::test]
    async fn test_mark_as_left_idempotent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[112; 32]);

        let (_, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        let first = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mark_as_left()
            .await
            .unwrap();
        let second = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mark_as_left()
            .await
            .unwrap();

        assert_eq!(first.removed_at, second.removed_at);
        assert!(second.self_removed);
    }

    #[tokio::test]
    async fn test_mark_as_removed_noop_after_left() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[113; 32]);

        let (_, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // User leaves first
        let left = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mark_as_left()
            .await
            .unwrap();
        assert!(left.self_removed);

        // Admin kick arrives later — must not overwrite self_removed
        let removed = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mark_as_removed()
            .await
            .unwrap();

        assert_eq!(left.removed_at, removed.removed_at);
        assert!(
            removed.self_removed,
            "self_removed must survive mark_as_removed no-op"
        );
    }

    #[tokio::test]
    async fn test_mark_as_left_emits_group_state_update() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[201; 32]);
        let (_, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        let mut updates = whitenoise
            .shared
            .group_state_stream_manager
            .subscribe(&account.pubkey, &group_id);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mark_as_left()
            .await
            .unwrap();

        assert_eq!(
            updates.try_recv().expect("should receive update"),
            GroupStateUpdate::LeftGroup,
        );
    }

    #[tokio::test]
    async fn test_mark_as_removed_emits_group_state_update() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[202; 32]);
        let (_, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        let mut updates = whitenoise
            .shared
            .group_state_stream_manager
            .subscribe(&account.pubkey, &group_id);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mark_as_removed()
            .await
            .unwrap();

        assert_eq!(
            updates.try_recv().expect("should receive update"),
            GroupStateUpdate::RemovedFromGroup,
        );
    }

    #[tokio::test]
    async fn test_group_state_only_emits_to_affected_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account_a = whitenoise.create_identity().await.unwrap();
        let account_b = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[210; 32]);
        whitenoise
            .require_session(&account_a.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();
        whitenoise
            .require_session(&account_b.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        let mut updates_a = whitenoise
            .shared
            .group_state_stream_manager
            .subscribe(&account_a.pubkey, &group_id);
        let mut updates_b = whitenoise
            .shared
            .group_state_stream_manager
            .subscribe(&account_b.pubkey, &group_id);

        whitenoise
            .require_session(&account_b.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mark_as_left()
            .await
            .unwrap();

        assert!(
            updates_a.try_recv().is_err(),
            "account A's stream must not receive a state event triggered by account B",
        );
        assert_eq!(
            updates_b
                .try_recv()
                .expect("account B must receive its own LeftGroup"),
            GroupStateUpdate::LeftGroup,
        );
    }

    #[tokio::test]
    async fn test_subscribe_to_group_state_returns_group_not_found_for_unknown_pair() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let unknown_group = GroupId::from_slice(&[220; 32]);

        let result = whitenoise
            .subscribe_to_group_state(&account.pubkey, &unknown_group)
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn test_mark_as_left_idempotent_does_not_double_emit() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[203; 32]);
        let (_, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        let mut updates = whitenoise
            .shared
            .group_state_stream_manager
            .subscribe(&account.pubkey, &group_id);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mark_as_left()
            .await
            .unwrap();
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .mark_as_left()
            .await
            .unwrap();

        let _first = updates.try_recv().expect("first call must emit");
        assert!(
            updates.try_recv().is_err(),
            "second mark_as_left is a no-op and must not emit again",
        );
    }

    #[tokio::test]
    async fn test_clear_chat_nonexistent_group_returns_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[140; 32]);

        let result = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .clear_chat()
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn test_chat_cleared_at_ms_returns_group_not_found_for_missing_row() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[141; 32]);

        // No AccountGroup row exists for this pubkey/group_id
        let result = AccountGroup::chat_cleared_at_ms(
            &account.pubkey,
            &group_id,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn test_clear_chat_sets_timestamp() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[142; 32]);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        let before = Utc::now();
        whitenoise
            .session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .clear_chat()
            .await
            .unwrap();
        let after = Utc::now();

        let ag = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap()
        .unwrap();

        let cleared_at = ag.chat_cleared_at.expect("chat_cleared_at must be set");
        // Allow 1s tolerance for millisecond truncation in the database layer
        let tolerance = chrono::Duration::seconds(1);
        assert!(
            cleared_at >= before - tolerance && cleared_at <= after + tolerance,
            "chat_cleared_at should be approximately between before and after timestamps"
        );
    }

    #[tokio::test]
    async fn test_clear_chat_resets_last_read_message_id() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[143; 32]);

        // Setup: create group_information (FK constraint) and account_group
        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Create a message and mark it as read
        let msg_id = EventId::from_hex(&format!("{:0>64}", "aab")).unwrap();
        let session = whitenoise.session(&account.pubkey).unwrap();
        AggregatedMessage::create_for_test(
            msg_id,
            group_id.clone(),
            account.pubkey,
            Utc::now(),
            &session.account_db.inner,
        )
        .await
        .unwrap();

        let ag = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .mark_message_read(&msg_id)
            .await
            .unwrap();
        assert_eq!(ag.last_read_message_id, Some(msg_id));

        // Clear the chat
        whitenoise
            .session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .clear_chat()
            .await
            .unwrap();

        // Verify last_read_message_id is reset
        let ag = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            ag.last_read_message_id.is_none(),
            "last_read_message_id must be None after clear_chat"
        );
    }

    /// Regression: post-phase-18e, `clear_chat` must drop this account's old
    /// `aggregated_messages` rows immediately. The pre-18e logic gated cleanup
    /// on a min-across-accounts `chat_cleared_at`, which after the per-account
    /// split silently turned the cleanup into a no-op whenever any other
    /// account in the group hadn't cleared its (totally separate) DB.
    #[tokio::test]
    async fn test_clear_chat_deletes_per_account_aggregated_messages() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[145; 32]);
        let session = whitenoise.session(&account.pubkey).unwrap();

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Seed two old messages directly in this account's per-account DB.
        let old = Utc::now() - chrono::Duration::seconds(10);
        for tag in &["e1", "e2"] {
            let id = EventId::from_hex(&format!("{:0>64}", tag)).unwrap();
            AggregatedMessage::create_for_test(
                id,
                group_id.clone(),
                account.pubkey,
                old,
                &session.account_db.inner,
            )
            .await
            .unwrap();
        }
        let before = AggregatedMessage::count_by_group(&group_id, &session.account_db.inner)
            .await
            .unwrap();
        assert_eq!(before, 2);

        session
            .membership()
            .for_group(&group_id)
            .clear_chat()
            .await
            .unwrap();

        let after = AggregatedMessage::count_by_group(&group_id, &session.account_db.inner)
            .await
            .unwrap();
        assert_eq!(
            after, 0,
            "clear_chat must drop this account's seeded messages even when no other account has cleared"
        );
    }

    #[tokio::test]
    async fn test_clear_chat_idempotent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[144; 32]);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // First clear
        whitenoise
            .session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .clear_chat()
            .await
            .unwrap();
        let ag1 = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap()
        .unwrap();
        let first_cleared_at = ag1.chat_cleared_at.unwrap();

        // Second clear should also succeed
        whitenoise
            .session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .clear_chat()
            .await
            .unwrap();
        let ag2 = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap()
        .unwrap();
        let second_cleared_at = ag2.chat_cleared_at.unwrap();

        assert!(
            second_cleared_at >= first_cleared_at,
            "second clear_chat must set a timestamp >= the first"
        );
    }

    #[tokio::test]
    async fn test_delete_chat_nonexistent_group_returns_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[150; 32]);

        let result = whitenoise.delete_chat(&account, &group_id).await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn test_delete_chat_removes_account_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[152; 32]);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        whitenoise.delete_chat(&account, &group_id).await.unwrap();

        let ag = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap();
        assert!(
            ag.is_none(),
            "account_group row must be gone after delete_chat"
        );
    }

    #[tokio::test]
    async fn test_delete_chat_hidden_from_visible_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[153; 32]);

        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        whitenoise.delete_chat(&account, &group_id).await.unwrap();

        let visible = AccountGroup::visible_for_account(&whitenoise, &account.pubkey)
            .await
            .unwrap();
        let group_ids: Vec<_> = visible.iter().map(|ag| ag.mls_group_id.clone()).collect();
        assert!(
            !group_ids.contains(&group_id),
            "deleted group must not appear in visible groups"
        );
    }

    #[tokio::test]
    async fn test_delete_chat_sole_account_removes_shared_data() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[154; 32]);

        // Create group_information (required FK) and account_group
        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        whitenoise.delete_chat(&account, &group_id).await.unwrap();

        // group_information must be deleted by delete_chat_data
        let gi_result =
            GroupInformation::find_by_mls_group_id(&group_id, &whitenoise.shared.database).await;
        assert!(
            gi_result.is_err(),
            "group_information must be deleted when sole account deletes"
        );

        // accounts_groups row must also be physically deleted
        let ag = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &account_pool(&whitenoise, &account.pubkey),
        )
        .await
        .unwrap();
        assert!(
            ag.is_none(),
            "account_group row must be deleted when sole account deletes"
        );
    }

    #[tokio::test]
    async fn test_delete_chat_multi_account_preserves_shared_data() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account_a = whitenoise.create_identity().await.unwrap();
        let account_b = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[155; 32]);

        // Create group_information and both account_groups
        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        whitenoise
            .require_session(&account_a.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();
        whitenoise
            .require_session(&account_b.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();

        // Account A deletes
        whitenoise.delete_chat(&account_a, &group_id).await.unwrap();

        // A's account_group row must be gone
        let ag_a = AccountGroup::find_by_account_and_group(
            &account_a.pubkey,
            &group_id,
            &account_pool(&whitenoise, &account_a.pubkey),
        )
        .await
        .unwrap();
        assert!(ag_a.is_none(), "A's row must be deleted");

        // B's account_group must be unchanged
        let ag_b = AccountGroup::find_by_account_and_group(
            &account_b.pubkey,
            &group_id,
            &account_pool(&whitenoise, &account_b.pubkey),
        )
        .await
        .unwrap();
        assert!(ag_b.is_some(), "B's row must still exist");

        // Shared data must still exist
        let gi =
            GroupInformation::find_by_mls_group_id(&group_id, &whitenoise.shared.database).await;
        assert!(
            gi.is_ok(),
            "group_information must persist while B still has the group"
        );

        // Account B deletes
        whitenoise.delete_chat(&account_b, &group_id).await.unwrap();

        // Now everything should be cleaned up
        let gi_after =
            GroupInformation::find_by_mls_group_id(&group_id, &whitenoise.shared.database).await;
        assert!(
            gi_after.is_err(),
            "group_information must be deleted after all accounts delete"
        );

        let ag_a_after = AccountGroup::find_by_account_and_group(
            &account_a.pubkey,
            &group_id,
            &account_pool(&whitenoise, &account_a.pubkey),
        )
        .await
        .unwrap();
        assert!(
            ag_a_after.is_none(),
            "A's row must be deleted after cleanup"
        );

        let ag_b_after = AccountGroup::find_by_account_and_group(
            &account_b.pubkey,
            &group_id,
            &account_pool(&whitenoise, &account_b.pubkey),
        )
        .await
        .unwrap();
        assert!(
            ag_b_after.is_none(),
            "B's row must be deleted after cleanup"
        );
    }

    #[test]
    fn test_is_message_visible() {
        let cleared_at = Utc::now();
        let before = cleared_at - chrono::Duration::seconds(10);
        let after = cleared_at + chrono::Duration::seconds(10);

        let mut ag = AccountGroup {
            id: None,
            account_pubkey: Keys::generate().public_key(),
            mls_group_id: GroupId::from_slice(&[0; 32]),
            user_confirmation: Some(true),
            welcomer_pubkey: None,
            last_read_message_id: None,
            pin_order: None,
            dm_peer_pubkey: None,
            archived_at: None,
            removed_at: None,
            self_removed: false,
            muted_until: None,
            chat_cleared_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        // No cleared-at: all messages visible
        assert!(ag.is_message_visible(before));
        assert!(ag.is_message_visible(cleared_at));
        assert!(ag.is_message_visible(after));

        // With cleared-at: only messages strictly after are visible
        ag.chat_cleared_at = Some(cleared_at);
        assert!(!ag.is_message_visible(before));
        assert!(!ag.is_message_visible(cleared_at));
        assert!(ag.is_message_visible(after));
    }
}

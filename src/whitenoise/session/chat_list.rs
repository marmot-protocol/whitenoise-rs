//! Chat list read operations scoped to an [`AccountSession`].

use std::collections::HashMap;
use std::path::PathBuf;

use mdk_core::prelude::*;
use nostr_sdk::PublicKey;

use super::AccountSession;
use crate::perf_instrument;
use crate::whitenoise::accounts_groups::AccountGroup;
use crate::whitenoise::aggregated_message::AggregatedMessage;
use crate::whitenoise::chat_list::{
    ChatListItem, assemble_chat_list_items, collect_pubkeys_to_fetch, sort_chat_list,
};
use crate::whitenoise::chat_list_streaming::{ChatListUpdate, ChatListUpdateTrigger};
use crate::whitenoise::error::Result;
use crate::whitenoise::group_information::{GroupInformation, GroupType};
use crate::whitenoise::groups::GroupWithMembership;
use crate::whitenoise::message_aggregator::ChatMessageSummary;
use crate::whitenoise::users::User;

/// View over [`AccountSession`] for chat list read operations.
///
/// Obtain via [`AccountSession::chat_list`].
pub struct ChatListOps<'a> {
    session: &'a AccountSession,
}

impl<'a> ChatListOps<'a> {
    pub(super) fn new(session: &'a AccountSession) -> Self {
        Self { session }
    }

    // ── Public API ─────────────────────────────────────────────────────

    /// Retrieve the active (non-archived) chat list for this account.
    ///
    /// Returns a list of chat summaries sorted by last activity (most recent
    /// first). Declined and archived groups are filtered out.
    #[perf_instrument("chat_list")]
    pub async fn active(&self) -> Result<Vec<ChatListItem>> {
        let visible = self.session.groups().visible().await?;
        let active: Vec<_> = visible
            .into_iter()
            .filter(|gwm| !gwm.membership.is_archived())
            .collect();
        self.build_for(active).await
    }

    /// Retrieve the archived chat list for this account.
    ///
    /// Returns only archived chats, sorted by last activity.
    #[perf_instrument("chat_list")]
    pub async fn archived(&self) -> Result<Vec<ChatListItem>> {
        let visible = self.session.groups().visible().await?;
        let archived: Vec<_> = visible
            .into_iter()
            .filter(|gwm| gwm.membership.is_archived())
            .collect();
        self.build_for(archived).await
    }

    /// Best-effort chat list stream notification for this account.
    ///
    /// Builds a chat list item for `group_id` and routes it to the active
    /// and/or archived stream managers based on `trigger` and the item's
    /// archive status. Errors are logged, never returned.
    #[perf_instrument("chat_list")]
    pub(crate) async fn emit_update(&self, group_id: &GroupId, trigger: ChatListUpdateTrigger) {
        let pubkey = &self.session.account_pubkey;
        let has_active = self
            .session
            .shared
            .chat_list_stream_manager
            .has_subscribers(pubkey);
        let has_archived = self
            .session
            .shared
            .archived_chat_list_stream_manager
            .has_subscribers(pubkey);
        if !has_active && !has_archived {
            return;
        }

        match self.build_item(group_id).await {
            Ok(Some(item)) => {
                let update = ChatListUpdate { trigger, item };
                match trigger {
                    ChatListUpdateTrigger::ChatArchiveChanged
                    | ChatListUpdateTrigger::ChatDeleted => {
                        if has_active {
                            self.session
                                .shared
                                .chat_list_stream_manager
                                .emit(pubkey, update.clone());
                        }
                        if has_archived {
                            self.session
                                .shared
                                .archived_chat_list_stream_manager
                                .emit(pubkey, update);
                        }
                    }
                    ChatListUpdateTrigger::RemovedFromGroup | ChatListUpdateTrigger::LeftGroup => {
                        if update.item.archived_at.is_some() {
                            if has_archived {
                                self.session
                                    .shared
                                    .archived_chat_list_stream_manager
                                    .emit(pubkey, update);
                            }
                        } else if has_active {
                            self.session
                                .shared
                                .chat_list_stream_manager
                                .emit(pubkey, update);
                        }
                    }
                    _ => {
                        if update.item.archived_at.is_some() {
                            if has_archived {
                                self.session
                                    .shared
                                    .archived_chat_list_stream_manager
                                    .emit(pubkey, update);
                            }
                        } else if has_active {
                            self.session
                                .shared
                                .chat_list_stream_manager
                                .emit(pubkey, update);
                        }
                    }
                }
            }
            Ok(None) => {
                tracing::debug!(
                    target: "whitenoise::session::chat_list",
                    "Skipped {:?} update for group {} - item not buildable",
                    trigger,
                    hex::encode(group_id.as_slice()),
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::session::chat_list",
                    "Failed to build chat list item for {:?} in group {}: {}",
                    trigger,
                    hex::encode(group_id.as_slice()),
                    e
                );
            }
        }
    }

    /// Build a single [`ChatListItem`] for a specific group.
    ///
    /// Returns `Ok(None)` if the group is not visible or not fully initialized.
    #[perf_instrument("chat_list")]
    pub(crate) async fn build_item(&self, group_id: &GroupId) -> Result<Option<ChatListItem>> {
        let Some(group) = self.session.mdk.get_group(group_id)? else {
            return Ok(None);
        };

        let account_group = AccountGroup::find_by_account_and_group(
            &self.session.account_pubkey,
            group_id,
            &self.session.shared.database,
        )
        .await?;
        let Some(membership) = account_group else {
            return Ok(None);
        };
        if !membership.is_visible() {
            return Ok(None);
        }

        let gwm = GroupWithMembership { group, membership };
        let mut items = self.build_for(vec![gwm]).await?;
        Ok(items.pop())
    }

    // ── Private helpers ────────────────────────────────────────────────

    /// Build a sorted chat list from a pre-filtered set of groups.
    #[perf_instrument("chat_list")]
    async fn build_for(
        &self,
        groups_with_membership: Vec<GroupWithMembership>,
    ) -> Result<Vec<ChatListItem>> {
        if groups_with_membership.is_empty() {
            return Ok(Vec::new());
        }

        let groups: Vec<_> = groups_with_membership
            .iter()
            .map(|gwm| gwm.group.clone())
            .collect();
        let group_ids: Vec<GroupId> = groups.iter().map(|g| g.mls_group_id.clone()).collect();

        let membership_map: HashMap<GroupId, AccountGroup> = groups_with_membership
            .iter()
            .map(|gwm| (gwm.group.mls_group_id.clone(), gwm.membership.clone()))
            .collect();

        let group_info_map = {
            let infos = GroupInformation::get_by_mls_group_ids_with_mdk(
                &group_ids,
                &self.session.mdk,
                &self.session.shared.database,
            )
            .await?;
            infos
                .into_iter()
                .map(|gi| (gi.mls_group_id.clone(), gi))
                .collect::<HashMap<_, _>>()
        };

        let dm_other_users: HashMap<GroupId, PublicKey> = AccountGroup::find_dm_peers_for_account(
            &self.session.account_pubkey,
            &self.session.shared.database,
        )
        .await?
        .into_iter()
        .collect();

        let last_message_map: HashMap<GroupId, ChatMessageSummary> =
            AggregatedMessage::find_last_by_group_ids(&group_ids, &self.session.shared.database)
                .await?
                .into_iter()
                .map(|s| (s.mls_group_id.clone(), s))
                .collect();

        let pubkeys_to_fetch = collect_pubkeys_to_fetch(&dm_other_users, &last_message_map);
        let users_by_pubkey: HashMap<PublicKey, User> =
            User::find_by_pubkeys(&pubkeys_to_fetch, &self.session.shared.database)
                .await?
                .into_iter()
                .map(|u| (u.pubkey, u))
                .collect();

        let image_paths = self.resolve_group_images(&groups, &group_info_map).await;

        let group_markers: Vec<_> = membership_map
            .iter()
            .map(|(gid, ag)| {
                let cleared_ms = ag.chat_cleared_at.map(|dt| dt.timestamp_millis());
                (gid.clone(), ag.last_read_message_id, cleared_ms)
            })
            .collect();
        let unread_counts = AggregatedMessage::count_unread_for_groups(
            &group_markers,
            &self.session.shared.database,
        )
        .await?;

        let mut items = assemble_chat_list_items(
            &groups,
            &group_info_map,
            &dm_other_users,
            &last_message_map,
            &users_by_pubkey,
            &image_paths,
            &membership_map,
            &unread_counts,
        );
        sort_chat_list(&mut items);

        Ok(items)
    }

    async fn resolve_group_images(
        &self,
        groups: &[group_types::Group],
        group_info_map: &HashMap<GroupId, GroupInformation>,
    ) -> HashMap<GroupId, PathBuf> {
        let group_ops = self.session.groups();
        let media = group_ops.media();
        let mut paths = HashMap::new();
        for group in groups.iter().filter(|g| {
            group_info_map
                .get(&g.mls_group_id)
                .map(|info| info.group_type == GroupType::Group)
                .unwrap_or(false)
        }) {
            match media.resolve_group_image_path(group).await {
                Ok(Some(path)) => {
                    paths.insert(group.mls_group_id.clone(), path);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::session::chat_list",
                        "Failed to resolve image for group {}: {}",
                        hex::encode(group.mls_group_id.as_slice()),
                        e
                    );
                }
            }
        }
        paths
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::Keys;

    use crate::whitenoise::session::test_helpers::test_session;

    #[tokio::test]
    async fn active_returns_empty_for_new_session() {
        let pk = Keys::generate().public_key();
        let session = test_session(pk).await;

        let items = session.chat_list().active().await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn archived_returns_empty_for_new_session() {
        let pk = Keys::generate().public_key();
        let session = test_session(pk).await;

        let items = session.chat_list().archived().await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn build_item_returns_none_for_nonexistent_group() {
        let pk = Keys::generate().public_key();
        let session = test_session(pk).await;
        let fake_group_id = mdk_core::prelude::GroupId::from_slice(&[0xAB; 32]);

        let result = session
            .chat_list()
            .build_item(&fake_group_id)
            .await
            .unwrap();
        assert!(result.is_none());
    }
}

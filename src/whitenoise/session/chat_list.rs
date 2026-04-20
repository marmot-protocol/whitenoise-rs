//! Chat list read operations scoped to an [`AccountSession`].

use std::collections::HashMap;
use std::path::PathBuf;

use mdk_core::prelude::*;
use nostr_sdk::PublicKey;

use super::AccountSession;
use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::accounts_groups::AccountGroup;
use crate::whitenoise::aggregated_message::AggregatedMessage;
use crate::whitenoise::chat_list::{
    ChatListItem, assemble_chat_list_items, collect_pubkeys_to_fetch, sort_chat_list,
};
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
            &self.session.database,
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
                &self.session.database,
            )
            .await?;
            infos
                .into_iter()
                .map(|gi| (gi.mls_group_id.clone(), gi))
                .collect::<HashMap<_, _>>()
        };

        let dm_other_users: HashMap<GroupId, PublicKey> = AccountGroup::find_dm_peers_for_account(
            &self.session.account_pubkey,
            &self.session.database,
        )
        .await?
        .into_iter()
        .collect();

        let last_message_map: HashMap<GroupId, ChatMessageSummary> =
            AggregatedMessage::find_last_by_group_ids(&group_ids, &self.session.database)
                .await?
                .into_iter()
                .map(|s| (s.mls_group_id.clone(), s))
                .collect();

        let pubkeys_to_fetch = collect_pubkeys_to_fetch(&dm_other_users, &last_message_map);
        let users_by_pubkey: HashMap<PublicKey, User> =
            User::find_by_pubkeys(&pubkeys_to_fetch, &self.session.database)
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
        let unread_counts =
            AggregatedMessage::count_unread_for_groups(&group_markers, &self.session.database)
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

    /// Transitionally calls `Whitenoise::get_instance()` for media resolution.
    /// Returns an empty map if the singleton is unavailable (e.g. in tests).
    async fn resolve_group_images(
        &self,
        groups: &[group_types::Group],
        group_info_map: &HashMap<GroupId, GroupInformation>,
    ) -> HashMap<GroupId, PathBuf> {
        let wn = match Whitenoise::get_instance() {
            Ok(wn) => wn,
            Err(_) => return HashMap::new(),
        };
        let account =
            match Account::find_by_pubkey(&self.session.account_pubkey, &self.session.database)
                .await
            {
                Ok(account) => account,
                Err(_) => return HashMap::new(),
            };

        let group_type_groups: Vec<_> = groups
            .iter()
            .filter(|g| {
                group_info_map
                    .get(&g.mls_group_id)
                    .map(|info| info.group_type == GroupType::Group)
                    .unwrap_or(false)
            })
            .cloned()
            .collect();

        wn.resolve_group_image_paths(&account, &group_type_groups)
            .await
    }
}

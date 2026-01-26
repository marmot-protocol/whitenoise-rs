use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use futures::future::join_all;
use mdk_core::prelude::*;
use nostr_sdk::PublicKey;
use serde::{Deserialize, Serialize};

use crate::whitenoise::{
    Whitenoise,
    accounts::Account,
    accounts_groups::AccountGroup,
    aggregated_message::AggregatedMessage,
    chat_list_streaming::{ChatListUpdate, ChatListUpdateTrigger},
    error::{Result, WhitenoiseError},
    group_information::{GroupInformation, GroupType},
    mdk_runner::run_mdk_operation,
    message_aggregator::ChatMessageSummary,
    users::User,
};

/// Summary of a chat/group for the chat list screen
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatListItem {
    /// MLS group identifier
    pub mls_group_id: GroupId,

    /// Display name for this chat:
    /// - Groups: The group name from MDK (may be empty string)
    /// - DMs: The other participant's display name (None if no metadata)
    pub name: Option<String>,

    /// Type of chat: Group or DirectMessage
    pub group_type: GroupType,

    /// When this group was created in our database (`DateTime<Utc>` for sorting consistency)
    pub created_at: DateTime<Utc>,

    /// Path to cached decrypted group image (Groups only, None for DMs)
    pub group_image_path: Option<PathBuf>,

    /// Profile picture URL of the other user (DMs only, None for Groups)
    /// From the other participant's metadata.picture
    pub group_image_url: Option<String>,

    /// Preview of the last message (None if no messages)
    pub last_message: Option<ChatMessageSummary>,

    /// Whether this group is pending user confirmation.
    /// `true` = user was invited but hasn't accepted/declined yet
    /// `false` = user has accepted (or created) this group
    pub pending_confirmation: bool,

    /// The public key of the user who invited the account to the group.
    pub welcomer_pubkey: Option<PublicKey>,

    /// Number of unread messages in this chat
    pub unread_count: usize,
}

impl ChatListItem {
    fn sort_key(&self) -> (Reverse<DateTime<Utc>>, &GroupId) {
        let timestamp = self
            .last_message
            .as_ref()
            .map(|m| m.created_at)
            .unwrap_or(self.created_at);
        (Reverse(timestamp), &self.mls_group_id)
    }
}

/// Resolves a user's display name from metadata.
///
/// Fallback chain: display_name -> name -> None
/// Does not fall back to truncated pubkey.
fn resolve_display_name(user: Option<&User>) -> Option<String> {
    user.and_then(|u| {
        u.metadata
            .display_name
            .as_ref()
            .filter(|s| !s.is_empty())
            .or(u.metadata.name.as_ref().filter(|s| !s.is_empty()))
    })
    .cloned()
}

/// Resolves the chat name based on group type.
///
/// - Groups: Returns the group name from MDK (may be empty string)
/// - DMs: Returns the other user's display name (None if no metadata)
fn resolve_chat_name(
    group: &group_types::Group,
    group_type: &GroupType,
    dm_other_user: Option<&User>,
) -> Option<String> {
    match group_type {
        GroupType::Group => Some(group.name.clone()),
        GroupType::DirectMessage => resolve_display_name(dm_other_user),
    }
}

/// Finds the "other user" in a DM group (the participant who isn't the account owner).
fn get_dm_other_user(group_members: &[PublicKey], account_pubkey: &PublicKey) -> Option<PublicKey> {
    group_members
        .iter()
        .find(|pk| *pk != account_pubkey)
        .copied()
}

/// Collects all pubkeys that need metadata lookup (DM participants + message authors).
fn collect_pubkeys_to_fetch(
    dm_other_users: &HashMap<GroupId, PublicKey>,
    last_message_map: &HashMap<GroupId, ChatMessageSummary>,
) -> Vec<PublicKey> {
    let mut pubkeys: HashSet<PublicKey> = dm_other_users.values().copied().collect();

    for summary in last_message_map.values() {
        pubkeys.insert(summary.author);
    }

    pubkeys.into_iter().collect()
}

/// Assembles ChatListItems from all the collected data.
fn assemble_chat_list_items(
    groups: &[group_types::Group],
    group_info_map: &HashMap<GroupId, GroupInformation>,
    dm_other_users: &HashMap<GroupId, PublicKey>,
    last_message_map: &HashMap<GroupId, ChatMessageSummary>,
    users_by_pubkey: &HashMap<PublicKey, User>,
    image_paths: &HashMap<GroupId, PathBuf>,
    membership_map: &HashMap<GroupId, AccountGroup>,
    unread_counts: &HashMap<GroupId, usize>,
) -> Vec<ChatListItem> {
    groups
        .iter()
        .filter_map(|group| {
            let group_info = group_info_map.get(&group.mls_group_id)?;

            let dm_other_user = dm_other_users
                .get(&group.mls_group_id)
                .and_then(|pk| users_by_pubkey.get(pk));

            let name = resolve_chat_name(group, &group_info.group_type, dm_other_user);

            let (group_image_path, group_image_url) = match group_info.group_type {
                GroupType::Group => (image_paths.get(&group.mls_group_id).cloned(), None),
                GroupType::DirectMessage => {
                    let url = dm_other_user
                        .and_then(|u| u.metadata.picture.as_ref().map(|url| url.to_string()));
                    (None, url)
                }
            };

            let last_message = last_message_map.get(&group.mls_group_id).map(|summary| {
                let mut msg = summary.clone();
                msg.author_display_name = resolve_display_name(users_by_pubkey.get(&msg.author));
                msg
            });

            let account_group = membership_map.get(&group.mls_group_id)?;
            let pending_confirmation = account_group.is_pending();
            let welcomer_pubkey = account_group.welcomer_pubkey;
            let unread_count = *unread_counts.get(&group.mls_group_id).unwrap_or(&0);

            Some(ChatListItem {
                mls_group_id: group.mls_group_id.clone(),
                name,
                group_type: group_info.group_type.clone(),
                created_at: group_info.created_at,
                group_image_path,
                group_image_url,
                last_message,
                pending_confirmation,
                welcomer_pubkey,
                unread_count,
            })
        })
        .collect()
}

/// Sorts chat list items by last activity (most recent first).
/// Groups without messages are sorted by creation date.
/// Uses mls_group_id as a tiebreaker for stable ordering when timestamps are identical.
pub(crate) fn sort_chat_list(items: &mut [ChatListItem]) {
    items.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
}

impl Whitenoise {
    /// Retrieves the chat list for an account.
    ///
    /// Returns a list of chat summaries sorted by last activity (most recent first).
    /// Groups without messages are sorted by creation date.
    /// Declined groups are filtered out.
    pub async fn get_chat_list(&self, account: &Account) -> Result<Vec<ChatListItem>> {
        // Use visible_groups() to filter out declined groups and get membership data
        let visible_groups = self.visible_groups(account).await?;
        if visible_groups.is_empty() {
            return Ok(Vec::new());
        }

        // Extract groups for existing helper functions
        let groups: Vec<_> = visible_groups.iter().map(|gwm| gwm.group.clone()).collect();
        let group_ids: Vec<GroupId> = groups.iter().map(|g| g.mls_group_id.clone()).collect();

        // Build pending status map from membership data
        let membership_map: HashMap<GroupId, AccountGroup> = visible_groups
            .iter()
            .map(|gwm| (gwm.group.mls_group_id.clone(), gwm.membership.clone()))
            .collect();

        let group_info_map = self
            .build_group_info_map(account.pubkey, &group_ids)
            .await?;
        let dm_other_users = self.identify_dm_participants(account, &groups, &group_info_map).await?;
        let last_message_map = self.build_last_message_map(&group_ids).await?;
        let pubkeys_to_fetch = collect_pubkeys_to_fetch(&dm_other_users, &last_message_map);
        let users_by_pubkey = self.build_users_by_pubkey(&pubkeys_to_fetch).await?;
        let image_paths = self
            .resolve_group_images(account, &groups, &group_info_map)
            .await;

        // Compute unread counts for all groups in a single batch query
        let group_markers: Vec<_> = membership_map
            .iter()
            .map(|(gid, ag)| (gid.clone(), ag.last_read_message_id))
            .collect();
        let unread_counts =
            AggregatedMessage::count_unread_for_groups(&group_markers, &self.database).await?;

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

    /// Builds a single ChatListItem for a specific group.
    ///
    /// Used by the streaming system to construct updates without re-fetching the entire chat list.
    /// Performs individual queries rather than batch operations.
    ///
    /// Returns `Ok(None)` if:
    /// - Group doesn't exist in MDK
    /// - GroupInformation doesn't exist (group not fully initialized)
    /// - AccountGroup is declined
    pub(crate) async fn build_chat_list_item(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<Option<ChatListItem>> {
        // 1. Get group and members from MDK in a separate thread
        let group_id_clone = group_id.clone();
        let (group_opt, members_opt) = run_mdk_operation(
            account.pubkey,
            &self.config.data_dir,
            move |mdk| {
                let group = mdk.get_group(&group_id_clone)?;
                let members = if group.is_some() {
                    Some(mdk.get_members(&group_id_clone)?.into_iter().collect::<Vec<PublicKey>>())
                } else {
                    None
                };
                Ok((group, members))
            },
        )
        .await?;
        
        let Some(group) = group_opt else {
            return Ok(None);
        };

        // 2. Get GroupInformation (returns error if not found)
        let group_info =
            match GroupInformation::find_by_mls_group_id(group_id, &self.database).await {
                Ok(info) => info,
                Err(_) => return Ok(None), // Group not fully initialized
            };

        // 3. Get AccountGroup for visibility/pending status and welcomer pubkey
        let account_group = AccountGroup::get(self, &account.pubkey, group_id).await?;
        let Some(account_group) = account_group else {
            return Ok(None); // No AccountGroup record
        };
        if !account_group.is_visible() {
            return Ok(None); // Declined
        }
        let pending_confirmation = account_group.is_pending();
        let welcomer_pubkey = account_group.welcomer_pubkey;

        // 4. For DMs: find other user, lookup metadata
        let dm_other_user = if group_info.group_type == GroupType::DirectMessage {
            if let Some(members) = members_opt {
                if let Some(other_pk) = get_dm_other_user(&members, &account.pubkey) {
                    User::find_by_pubkey(&other_pk, &self.database).await.ok()
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        // 5. Get last message
        let last_message_summaries = AggregatedMessage::find_last_by_group_ids(
            std::slice::from_ref(group_id),
            &self.database,
        )
        .await?;
        let last_message_summary = last_message_summaries.into_iter().next();

        // 6. Lookup message author metadata and build final last_message
        let last_message = if let Some(mut summary) = last_message_summary {
            let author_user = User::find_by_pubkey(&summary.author, &self.database)
                .await
                .ok();
            summary.author_display_name = resolve_display_name(author_user.as_ref());
            Some(summary)
        } else {
            None
        };

        // 7. Resolve name and image based on group type
        let name = resolve_chat_name(&group, &group_info.group_type, dm_other_user.as_ref());

        let (group_image_path, group_image_url) = match group_info.group_type {
            GroupType::Group => {
                let path = self
                    .resolve_group_image_path(account, &group)
                    .await
                    .ok()
                    .flatten();
                (path, None)
            }
            GroupType::DirectMessage => {
                let url = dm_other_user
                    .as_ref()
                    .and_then(|u| u.metadata.picture.as_ref().map(|url| url.to_string()));
                (None, url)
            }
        };

        // 8. Compute unread count
        let unread_count = AggregatedMessage::count_unread_for_group(
            group_id,
            account_group.last_read_message_id.as_ref(),
            &self.database,
        )
        .await?;

        // 9. Assemble and return ChatListItem
        Ok(Some(ChatListItem {
            mls_group_id: group_id.clone(),
            name,
            group_type: group_info.group_type,
            created_at: group_info.created_at,
            group_image_path,
            group_image_url,
            last_message,
            pending_confirmation,
            welcomer_pubkey,
            unread_count,
        }))
    }

    /// Emit a chat list update with the given trigger for a specific account.
    ///
    /// Checks for subscribers first to avoid expensive `build_chat_list_item` calls.
    /// Errors are logged but don't affect the caller.
    pub(crate) async fn emit_chat_list_update(
        &self,
        account: &Account,
        group_id: &GroupId,
        trigger: ChatListUpdateTrigger,
    ) {
        if !self
            .chat_list_stream_manager
            .has_subscribers(&account.pubkey)
        {
            return;
        }

        self.emit_chat_list_update_for_account(&account.pubkey, group_id, trigger)
            .await;
    }

    /// Emit a chat list update to ALL subscribed accounts in a group.
    ///
    /// Use this when a shared database modification (like deletion) triggers an
    /// update that should reach all subscribed accounts, regardless of which
    /// account's handler detected the change.
    ///
    /// This is necessary because event handlers process accounts sequentially,
    /// and the first handler modifies shared state. Only the first handler can
    /// correctly detect certain conditions (e.g., "was the deleted message the
    /// last message?"), so it must emit for all subscribers.
    pub(crate) async fn emit_chat_list_update_for_group(
        &self,
        group_id: &GroupId,
        trigger: ChatListUpdateTrigger,
    ) {
        let account_groups = match AccountGroup::find_by_group(group_id, &self.database).await {
            Ok(groups) => groups,
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::chat_list_streaming",
                    "Failed to find accounts in group {}: {}",
                    hex::encode(group_id.as_slice()),
                    e
                );
                return;
            }
        };

        for ag in account_groups {
            if self
                .chat_list_stream_manager
                .has_subscribers(&ag.account_pubkey)
            {
                self.emit_chat_list_update_for_account(&ag.account_pubkey, group_id, trigger)
                    .await;
            }
        }
    }

    /// Internal helper to emit a chat list update for a specific account pubkey.
    async fn emit_chat_list_update_for_account(
        &self,
        pubkey: &PublicKey,
        group_id: &GroupId,
        trigger: ChatListUpdateTrigger,
    ) {
        let account = match Account::find_by_pubkey(pubkey, &self.database).await {
            Ok(acc) => acc,
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::chat_list_streaming",
                    "Failed to find account {} for chat list update: {}",
                    pubkey,
                    e
                );
                return;
            }
        };

        match self.build_chat_list_item(&account, group_id).await {
            Ok(Some(item)) => {
                self.chat_list_stream_manager
                    .emit(pubkey, ChatListUpdate { trigger, item });
            }
            Ok(None) => {
                tracing::debug!(
                    target: "whitenoise::chat_list_streaming",
                    "Skipped {:?} update for group {} - item not buildable",
                    trigger,
                    hex::encode(group_id.as_slice()),
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::chat_list_streaming",
                    "Failed to build chat list item for {:?} in group {}: {}",
                    trigger,
                    hex::encode(group_id.as_slice()),
                    e
                );
            }
        }
    }

    async fn build_group_info_map(
        &self,
        account_pubkey: PublicKey,
        group_ids: &[GroupId],
    ) -> Result<HashMap<GroupId, GroupInformation>> {
        let group_infos =
            GroupInformation::get_by_mls_group_ids(account_pubkey, group_ids, self).await?;
        Ok(group_infos
            .into_iter()
            .map(|gi| (gi.mls_group_id.clone(), gi))
            .collect())
    }

    /// Identifies the "other user" in each DM group.
    async fn identify_dm_participants(
        &self,
        account: &Account,
        groups: &[group_types::Group],
        group_info_map: &HashMap<GroupId, GroupInformation>,
    ) -> Result<HashMap<GroupId, PublicKey>> {
        // Collect DM groups that need member lookup
        let dm_groups: Vec<_> = groups
            .iter()
            .filter(|group| {
                group_info_map
                    .get(&group.mls_group_id)
                    .map(|info| info.group_type == GroupType::DirectMessage)
                    .unwrap_or(false)
            })
            .cloned()
            .collect();

        // Batch all MDK get_members calls in a separate thread
        let account_pubkey = account.pubkey;
        let dm_other_users = run_mdk_operation(
            account.pubkey,
            &self.config.data_dir,
            move |mdk| {
                let mut dm_other_users = HashMap::new();
                for group in dm_groups {
                    let members: Vec<PublicKey> =
                        mdk.get_members(&group.mls_group_id)?.into_iter().collect();
                    if let Some(other_pk) = get_dm_other_user(&members, &account_pubkey) {
                        dm_other_users.insert(group.mls_group_id.clone(), other_pk);
                    }
                }
                Ok(dm_other_users)
            },
        )
        .await?;

        Ok(dm_other_users)
    }

    async fn build_last_message_map(
        &self,
        group_ids: &[GroupId],
    ) -> Result<HashMap<GroupId, ChatMessageSummary>> {
        let summaries =
            AggregatedMessage::find_last_by_group_ids(group_ids, &self.database).await?;
        Ok(summaries
            .into_iter()
            .map(|s| (s.mls_group_id.clone(), s))
            .collect())
    }

    async fn build_users_by_pubkey(
        &self,
        pubkeys: &[PublicKey],
    ) -> Result<HashMap<PublicKey, User>> {
        let users = User::find_by_pubkeys(pubkeys, &self.database).await?;
        Ok(users.into_iter().map(|u| (u.pubkey, u)).collect())
    }

    /// Resolves image paths for Group-type chats only (DMs use profile picture URLs).
    async fn resolve_group_images(
        &self,
        account: &Account,
        groups: &[group_types::Group],
        group_info_map: &HashMap<GroupId, GroupInformation>,
    ) -> HashMap<GroupId, PathBuf> {
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

        self.resolve_group_image_paths(account, &group_type_groups)
            .await
    }

    /// Resolves image paths for multiple groups in parallel.
    ///
    /// Directly uses the groups already fetched from MDK, avoiding
    /// redundant MDK instantiation and group fetching per group.
    ///
    /// Groups without images return None (not an error).
    /// Download failures are logged but don't fail the batch.
    async fn resolve_group_image_paths(
        &self,
        account: &Account,
        groups: &[group_types::Group],
    ) -> HashMap<GroupId, PathBuf> {
        let futures = groups.iter().map(|group| {
            let group_id = group.mls_group_id.clone();
            async move {
                let result = self.resolve_group_image_path(account, group).await;
                (group_id, result)
            }
        });

        let results = join_all(futures).await;

        let mut paths = HashMap::new();
        for (group_id, result) in results {
            match result {
                Ok(Some(path)) => {
                    paths.insert(group_id, path);
                }
                Ok(None) => {
                    // No image configured - normal, not an error
                }
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::chat_list",
                        "Failed to resolve image for group {}: {}",
                        hex::encode(group_id.as_slice()),
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
    use super::*;
    use crate::whitenoise::aggregated_message::AggregatedMessage;
    use crate::whitenoise::message_aggregator::ChatMessage;
    use crate::whitenoise::test_utils::{create_mock_whitenoise, create_nostr_group_config_data};
    use nostr_sdk::{Metadata, Timestamp};

    #[tokio::test]
    async fn test_get_chat_list_empty() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let chat_list = whitenoise.get_chat_list(&account).await.unwrap();
        assert!(chat_list.is_empty());
    }

    #[tokio::test]
    async fn test_get_chat_list_single_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let config = create_nostr_group_config_data(vec![creator.pubkey]);
        let _group = whitenoise
            .create_group(&creator, vec![member.pubkey], config, None)
            .await
            .unwrap();

        let chat_list = whitenoise.get_chat_list(&creator).await.unwrap();

        assert_eq!(chat_list.len(), 1);
        assert_eq!(chat_list[0].group_type, GroupType::Group);
        assert_eq!(chat_list[0].name, Some("Test group".to_string()));
        assert!(chat_list[0].last_message.is_none());
        assert!(!chat_list[0].pending_confirmation);
        assert!(chat_list[0].welcomer_pubkey.is_none());
    }

    #[tokio::test]
    async fn test_get_chat_list_dm_without_other_user_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let mut member_user = User::find_by_pubkey(&member.pubkey, &whitenoise.database)
            .await
            .unwrap();
        member_user.metadata = Metadata::new();
        member_user.save(&whitenoise.database).await.unwrap();

        let mut config = create_nostr_group_config_data(vec![creator.pubkey, member.pubkey]);
        config.name = String::new();
        let _group = whitenoise
            .create_group(
                &creator,
                vec![member.pubkey],
                config,
                Some(GroupType::DirectMessage),
            )
            .await
            .unwrap();

        let chat_list = whitenoise.get_chat_list(&creator).await.unwrap();

        assert_eq!(chat_list.len(), 1);
        assert_eq!(chat_list[0].group_type, GroupType::DirectMessage);
        assert!(
            chat_list[0].name.is_none(),
            "Expected DM name to be None, got: {:?}",
            chat_list[0].name
        );
        assert!(!chat_list[0].pending_confirmation);
    }

    #[tokio::test]
    async fn test_get_chat_list_dm_with_display_name() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let mut user = User::find_by_pubkey(&member.pubkey, &whitenoise.database)
            .await
            .unwrap();
        user.metadata = Metadata::new().display_name("Bob Display").name("Bob Name");
        user.save(&whitenoise.database).await.unwrap();

        let mut config = create_nostr_group_config_data(vec![creator.pubkey, member.pubkey]);
        config.name = String::new();
        let _group = whitenoise
            .create_group(
                &creator,
                vec![member.pubkey],
                config,
                Some(GroupType::DirectMessage),
            )
            .await
            .unwrap();

        let chat_list = whitenoise.get_chat_list(&creator).await.unwrap();

        assert_eq!(chat_list.len(), 1);
        // Should use display_name, not name
        assert_eq!(chat_list[0].name, Some("Bob Display".to_string()));
        assert!(!chat_list[0].pending_confirmation);
    }

    #[tokio::test]
    async fn test_get_chat_list_dm_falls_back_to_name() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let mut user = User::find_by_pubkey(&member.pubkey, &whitenoise.database)
            .await
            .unwrap();
        user.metadata = Metadata::new().name("Bob Name");
        user.save(&whitenoise.database).await.unwrap();

        let mut config = create_nostr_group_config_data(vec![creator.pubkey, member.pubkey]);
        config.name = String::new();
        let _group = whitenoise
            .create_group(
                &creator,
                vec![member.pubkey],
                config,
                Some(GroupType::DirectMessage),
            )
            .await
            .unwrap();

        let chat_list = whitenoise.get_chat_list(&creator).await.unwrap();

        assert_eq!(chat_list.len(), 1);
        assert_eq!(chat_list[0].name, Some("Bob Name".to_string()));
        assert!(!chat_list[0].pending_confirmation);
    }

    #[tokio::test]
    async fn test_get_chat_list_dm_skips_empty_display_name() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let mut user = User::find_by_pubkey(&member.pubkey, &whitenoise.database)
            .await
            .unwrap();
        let mut metadata = Metadata::new().name("Fallback Name");
        metadata.display_name = Some(String::new());
        user.metadata = metadata;
        user.save(&whitenoise.database).await.unwrap();

        let mut config = create_nostr_group_config_data(vec![creator.pubkey, member.pubkey]);
        config.name = String::new();
        let _group = whitenoise
            .create_group(
                &creator,
                vec![member.pubkey],
                config,
                Some(GroupType::DirectMessage),
            )
            .await
            .unwrap();

        let chat_list = whitenoise.get_chat_list(&creator).await.unwrap();

        assert_eq!(chat_list.len(), 1);
        assert_eq!(chat_list[0].name, Some("Fallback Name".to_string()));
        assert!(!chat_list[0].pending_confirmation);
    }

    #[tokio::test]
    async fn test_get_chat_list_multiple_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member1 = whitenoise.create_identity().await.unwrap();
        let member2 = whitenoise.create_identity().await.unwrap();

        let mut config1 = create_nostr_group_config_data(vec![creator.pubkey]);
        config1.name = "First Group".to_string();
        let _group1 = whitenoise
            .create_group(
                &creator,
                vec![member1.pubkey],
                config1,
                Some(GroupType::Group),
            )
            .await
            .unwrap();

        let mut config2 = create_nostr_group_config_data(vec![creator.pubkey]);
        config2.name = "Second Group".to_string();
        let _group2 = whitenoise
            .create_group(
                &creator,
                vec![member2.pubkey],
                config2,
                Some(GroupType::Group),
            )
            .await
            .unwrap();

        let chat_list = whitenoise.get_chat_list(&creator).await.unwrap();

        assert_eq!(chat_list.len(), 2);
        assert!(chat_list.iter().all(|c| c.group_type == GroupType::Group));
        assert!(chat_list.iter().all(|c| !c.pending_confirmation));
    }

    #[tokio::test]
    async fn test_get_chat_list_sorting_by_created_at() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member1 = whitenoise.create_identity().await.unwrap();
        let member2 = whitenoise.create_identity().await.unwrap();

        let mut config1 = create_nostr_group_config_data(vec![creator.pubkey]);
        config1.name = "First".to_string();
        let _group1 = whitenoise
            .create_group(&creator, vec![member1.pubkey], config1, None)
            .await
            .unwrap();

        // Small delay to ensure different timestamps
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let mut config2 = create_nostr_group_config_data(vec![creator.pubkey]);
        config2.name = "Second".to_string();
        let _group2 = whitenoise
            .create_group(&creator, vec![member2.pubkey], config2, None)
            .await
            .unwrap();

        let chat_list = whitenoise.get_chat_list(&creator).await.unwrap();

        assert_eq!(chat_list.len(), 2);
        assert_eq!(chat_list[0].name, Some("Second".to_string()));
        assert_eq!(chat_list[1].name, Some("First".to_string()));
        assert!(chat_list.iter().all(|c| !c.pending_confirmation));
    }

    #[tokio::test]
    async fn test_get_chat_list_sorting_by_last_message() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member1 = whitenoise.create_identity().await.unwrap();
        let member2 = whitenoise.create_identity().await.unwrap();

        let mut config1 = create_nostr_group_config_data(vec![creator.pubkey]);
        config1.name = "Old Message Group".to_string();
        let group1 = whitenoise
            .create_group(
                &creator,
                vec![member1.pubkey],
                config1,
                Some(GroupType::Group),
            )
            .await
            .unwrap();

        // Small delay
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let mut config2 = create_nostr_group_config_data(vec![creator.pubkey]);
        config2.name = "New Message Group".to_string();
        let group2 = whitenoise
            .create_group(
                &creator,
                vec![member2.pubkey],
                config2,
                Some(GroupType::Group),
            )
            .await
            .unwrap();

        let msg1 = ChatMessage {
            id: format!("{:0>64}", "1"),
            author: creator.pubkey,
            content: "Old".to_string(),
            created_at: Timestamp::from(1000),
            tags: nostr_sdk::Tags::new(),
            is_reply: false,
            reply_to_id: None,
            is_deleted: false,
            content_tokens: vec![],
            reactions: Default::default(),
            kind: 9,
            media_attachments: vec![],
        };
        AggregatedMessage::insert_message(&msg1, &group1.mls_group_id, &whitenoise.database)
            .await
            .unwrap();

        let msg2 = ChatMessage {
            id: format!("{:0>64}", "2"),
            author: creator.pubkey,
            content: "New".to_string(),
            created_at: Timestamp::from(2000),
            tags: nostr_sdk::Tags::new(),
            is_reply: false,
            reply_to_id: None,
            is_deleted: false,
            content_tokens: vec![],
            reactions: Default::default(),
            kind: 9,
            media_attachments: vec![],
        };
        AggregatedMessage::insert_message(&msg2, &group2.mls_group_id, &whitenoise.database)
            .await
            .unwrap();

        let chat_list = whitenoise.get_chat_list(&creator).await.unwrap();

        assert_eq!(chat_list.len(), 2);
        assert_eq!(chat_list[0].name, Some("New Message Group".to_string()));
        assert_eq!(chat_list[1].name, Some("Old Message Group".to_string()));
        assert!(chat_list.iter().all(|c| !c.pending_confirmation));
    }

    #[tokio::test]
    async fn test_get_chat_list_mixed_group_and_dm() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member1 = whitenoise.create_identity().await.unwrap();
        let member2 = whitenoise.create_identity().await.unwrap();

        let config1 = create_nostr_group_config_data(vec![creator.pubkey]);
        let _group1 = whitenoise
            .create_group(
                &creator,
                vec![member1.pubkey],
                config1,
                Some(GroupType::Group),
            )
            .await
            .unwrap();

        let mut config2 = create_nostr_group_config_data(vec![creator.pubkey, member2.pubkey]);
        config2.name = String::new();
        let _group2 = whitenoise
            .create_group(
                &creator,
                vec![member2.pubkey],
                config2,
                Some(GroupType::DirectMessage),
            )
            .await
            .unwrap();

        let chat_list = whitenoise.get_chat_list(&creator).await.unwrap();

        assert_eq!(chat_list.len(), 2);
        let group_count = chat_list
            .iter()
            .filter(|c| c.group_type == GroupType::Group)
            .count();
        let dm_count = chat_list
            .iter()
            .filter(|c| c.group_type == GroupType::DirectMessage)
            .count();
        assert_eq!(group_count, 1);
        assert_eq!(dm_count, 1);
        assert!(chat_list.iter().all(|c| !c.pending_confirmation));
    }

    #[tokio::test]
    async fn test_get_chat_list_dm_shows_other_user_picture_url() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let mut user = User::find_by_pubkey(&member.pubkey, &whitenoise.database)
            .await
            .unwrap();
        user.metadata = user
            .metadata
            .picture(nostr_sdk::Url::parse("https://example.com/pic.jpg").unwrap());
        user.save(&whitenoise.database).await.unwrap();

        let mut config = create_nostr_group_config_data(vec![creator.pubkey, member.pubkey]);
        config.name = String::new();
        let _group = whitenoise
            .create_group(
                &creator,
                vec![member.pubkey],
                config,
                Some(GroupType::DirectMessage),
            )
            .await
            .unwrap();

        let chat_list = whitenoise.get_chat_list(&creator).await.unwrap();

        assert_eq!(chat_list.len(), 1);
        assert_eq!(chat_list[0].group_type, GroupType::DirectMessage);
        assert_eq!(
            chat_list[0].group_image_url,
            Some("https://example.com/pic.jpg".to_string())
        );
        assert!(chat_list[0].group_image_path.is_none());
        assert!(!chat_list[0].pending_confirmation);
    }

    #[tokio::test]
    async fn test_get_chat_list_group_has_no_image_url() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let config = create_nostr_group_config_data(vec![creator.pubkey]);
        let _group = whitenoise
            .create_group(
                &creator,
                vec![member.pubkey],
                config,
                Some(GroupType::Group),
            )
            .await
            .unwrap();

        let chat_list = whitenoise.get_chat_list(&creator).await.unwrap();

        assert_eq!(chat_list.len(), 1);
        assert_eq!(chat_list[0].group_type, GroupType::Group);
        assert!(chat_list[0].group_image_url.is_none());
        assert!(!chat_list[0].pending_confirmation);
    }

    #[tokio::test]
    async fn test_get_chat_list_last_message_author_display_name() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let mut user = User::find_by_pubkey(&creator.pubkey, &whitenoise.database)
            .await
            .unwrap();
        user.metadata = user.metadata.display_name("Alice");
        user.save(&whitenoise.database).await.unwrap();

        let config = create_nostr_group_config_data(vec![creator.pubkey]);
        let group = whitenoise
            .create_group(
                &creator,
                vec![member.pubkey],
                config,
                Some(GroupType::Group),
            )
            .await
            .unwrap();

        use crate::whitenoise::message_aggregator::ChatMessage;
        let msg = ChatMessage {
            id: format!("{:0>64}", "1"),
            author: creator.pubkey,
            content: "Hello".to_string(),
            created_at: Timestamp::now(),
            tags: nostr_sdk::Tags::new(),
            is_reply: false,
            reply_to_id: None,
            is_deleted: false,
            content_tokens: vec![],
            reactions: Default::default(),
            kind: 9,
            media_attachments: vec![],
        };
        AggregatedMessage::insert_message(&msg, &group.mls_group_id, &whitenoise.database)
            .await
            .unwrap();

        let chat_list = whitenoise.get_chat_list(&creator).await.unwrap();

        assert_eq!(chat_list.len(), 1);
        let last_msg = chat_list[0].last_message.as_ref().unwrap();
        assert_eq!(last_msg.author_display_name, Some("Alice".to_string()));
        assert_eq!(last_msg.content, "Hello");
        assert!(!chat_list[0].pending_confirmation);
    }

    #[tokio::test]
    async fn test_get_chat_list_sorting_mixed_messages_and_no_messages() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member1 = whitenoise.create_identity().await.unwrap();
        let member2 = whitenoise.create_identity().await.unwrap();

        let mut config1 = create_nostr_group_config_data(vec![creator.pubkey]);
        config1.name = "Old Message".to_string();
        let group1 = whitenoise
            .create_group(
                &creator,
                vec![member1.pubkey],
                config1,
                Some(GroupType::Group),
            )
            .await
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let mut config2 = create_nostr_group_config_data(vec![creator.pubkey]);
        config2.name = "No Message".to_string();
        let _group2 = whitenoise
            .create_group(
                &creator,
                vec![member2.pubkey],
                config2,
                Some(GroupType::Group),
            )
            .await
            .unwrap();

        use crate::whitenoise::message_aggregator::ChatMessage;
        let msg = ChatMessage {
            id: format!("{:0>64}", "1"),
            author: creator.pubkey,
            content: "Old".to_string(),
            created_at: Timestamp::from(1000),
            tags: nostr_sdk::Tags::new(),
            is_reply: false,
            reply_to_id: None,
            is_deleted: false,
            content_tokens: vec![],
            reactions: Default::default(),
            kind: 9,
            media_attachments: vec![],
        };
        AggregatedMessage::insert_message(&msg, &group1.mls_group_id, &whitenoise.database)
            .await
            .unwrap();

        let chat_list = whitenoise.get_chat_list(&creator).await.unwrap();

        assert_eq!(chat_list.len(), 2);
        assert_eq!(chat_list[0].name, Some("No Message".to_string()));
        assert_eq!(chat_list[1].name, Some("Old Message".to_string()));
        assert!(chat_list.iter().all(|c| !c.pending_confirmation));
    }

    #[tokio::test]
    async fn test_build_chat_list_item_returns_none_for_nonexistent_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let nonexistent_group_id = mdk_core::prelude::GroupId::from_slice(&[99; 32]);
        let result = whitenoise
            .build_chat_list_item(&account, &nonexistent_group_id)
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_build_chat_list_item_returns_valid_item() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let config = create_nostr_group_config_data(vec![creator.pubkey]);
        let group = whitenoise
            .create_group(&creator, vec![member.pubkey], config, None)
            .await
            .unwrap();

        let result = whitenoise
            .build_chat_list_item(&creator, &group.mls_group_id)
            .await
            .unwrap();

        assert!(result.is_some());
        let item = result.unwrap();
        assert_eq!(item.mls_group_id, group.mls_group_id);
        assert_eq!(item.name, Some("Test group".to_string()));
        assert_eq!(item.group_type, GroupType::Group);
        assert!(item.last_message.is_none());
        assert!(!item.pending_confirmation);
        assert!(item.welcomer_pubkey.is_none());
    }

    #[tokio::test]
    async fn test_build_chat_list_item_with_last_message() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        // Set author display name
        let mut user = User::find_by_pubkey(&creator.pubkey, &whitenoise.database)
            .await
            .unwrap();
        user.metadata = user.metadata.display_name("Alice");
        user.save(&whitenoise.database).await.unwrap();

        let config = create_nostr_group_config_data(vec![creator.pubkey]);
        let group = whitenoise
            .create_group(&creator, vec![member.pubkey], config, None)
            .await
            .unwrap();

        let msg = ChatMessage {
            id: format!("{:0>64}", "1"),
            author: creator.pubkey,
            content: "Hello World".to_string(),
            created_at: Timestamp::now(),
            tags: nostr_sdk::Tags::new(),
            is_reply: false,
            reply_to_id: None,
            is_deleted: false,
            content_tokens: vec![],
            reactions: Default::default(),
            kind: 9,
            media_attachments: vec![],
        };
        AggregatedMessage::insert_message(&msg, &group.mls_group_id, &whitenoise.database)
            .await
            .unwrap();

        let result = whitenoise
            .build_chat_list_item(&creator, &group.mls_group_id)
            .await
            .unwrap();

        assert!(result.is_some());
        let item = result.unwrap();
        assert!(item.last_message.is_some());
        let last_msg = item.last_message.unwrap();
        assert_eq!(last_msg.content, "Hello World");
        assert_eq!(last_msg.author_display_name, Some("Alice".to_string()));
    }

    #[tokio::test]
    async fn test_build_chat_list_item_dm_resolves_other_user_name() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        // Set other user's display name
        let mut user = User::find_by_pubkey(&member.pubkey, &whitenoise.database)
            .await
            .unwrap();
        user.metadata = Metadata::new().display_name("Bob");
        user.save(&whitenoise.database).await.unwrap();

        let mut config = create_nostr_group_config_data(vec![creator.pubkey, member.pubkey]);
        config.name = String::new();
        let group = whitenoise
            .create_group(
                &creator,
                vec![member.pubkey],
                config,
                Some(GroupType::DirectMessage),
            )
            .await
            .unwrap();

        let result = whitenoise
            .build_chat_list_item(&creator, &group.mls_group_id)
            .await
            .unwrap();

        assert!(result.is_some());
        let item = result.unwrap();
        assert_eq!(item.group_type, GroupType::DirectMessage);
        assert_eq!(item.name, Some("Bob".to_string()));
    }

    #[test]
    fn test_sort_chat_list_uses_group_id_tiebreaker_without_messages() {
        use chrono::TimeZone;

        let timestamp = Utc.timestamp_opt(1000, 0).unwrap();

        // Create items in WRONG order with identical timestamps
        // Without tiebreaker, they'd stay in this wrong order
        let mut items = vec![
            ChatListItem {
                mls_group_id: GroupId::from_slice(&[3u8; 32]),
                name: Some("Group C".to_string()),
                group_type: GroupType::Group,
                created_at: timestamp,
                group_image_path: None,
                group_image_url: None,
                last_message: None,
                pending_confirmation: false,
                welcomer_pubkey: None,
                unread_count: 0,
            },
            ChatListItem {
                mls_group_id: GroupId::from_slice(&[1u8; 32]),
                name: Some("Group A".to_string()),
                group_type: GroupType::Group,
                created_at: timestamp,
                group_image_path: None,
                group_image_url: None,
                last_message: None,
                pending_confirmation: false,
                welcomer_pubkey: None,
                unread_count: 0,
            },
            ChatListItem {
                mls_group_id: GroupId::from_slice(&[2u8; 32]),
                name: Some("Group B".to_string()),
                group_type: GroupType::Group,
                created_at: timestamp,
                group_image_path: None,
                group_image_url: None,
                last_message: None,
                pending_confirmation: false,
                welcomer_pubkey: None,
                unread_count: 0,
            },
        ];

        sort_chat_list(&mut items);

        // After sorting, should be ordered by group_id (ascending)
        // This FAILS without the tiebreaker because items stay in original order
        assert_eq!(items[0].mls_group_id.as_slice(), &[1u8; 32]);
        assert_eq!(items[1].mls_group_id.as_slice(), &[2u8; 32]);
        assert_eq!(items[2].mls_group_id.as_slice(), &[3u8; 32]);
    }

    #[test]
    fn test_sort_chat_list_uses_group_id_tiebreaker_with_identical_message_timestamps() {
        use chrono::TimeZone;

        let timestamp = Utc.timestamp_opt(1000, 0).unwrap();
        let msg_timestamp = Utc.timestamp_opt(2000, 0).unwrap();

        // Create items with same message timestamp but different group IDs
        let mut items = vec![
            ChatListItem {
                mls_group_id: GroupId::from_slice(&[255u8; 32]),
                name: Some("Group Last".to_string()),
                group_type: GroupType::Group,
                created_at: timestamp,
                group_image_path: None,
                group_image_url: None,
                last_message: Some(ChatMessageSummary {
                    message_id: nostr_sdk::EventId::all_zeros(),
                    mls_group_id: GroupId::from_slice(&[255u8; 32]),
                    author: nostr_sdk::Keys::generate().public_key(),
                    author_display_name: None,
                    content: "Test".to_string(),
                    created_at: msg_timestamp,
                    media_attachment_count: 0,
                }),
                pending_confirmation: false,
                welcomer_pubkey: None,
                unread_count: 0,
            },
            ChatListItem {
                mls_group_id: GroupId::from_slice(&[1u8; 32]),
                name: Some("Group First".to_string()),
                group_type: GroupType::Group,
                created_at: timestamp,
                group_image_path: None,
                group_image_url: None,
                last_message: Some(ChatMessageSummary {
                    message_id: nostr_sdk::EventId::all_zeros(),
                    mls_group_id: GroupId::from_slice(&[1u8; 32]),
                    author: nostr_sdk::Keys::generate().public_key(),
                    author_display_name: None,
                    content: "Test".to_string(),
                    created_at: msg_timestamp,
                    media_attachment_count: 0,
                }),
                pending_confirmation: false,
                welcomer_pubkey: None,
                unread_count: 0,
            },
            ChatListItem {
                mls_group_id: GroupId::from_slice(&[128u8; 32]),
                name: Some("Group Middle".to_string()),
                group_type: GroupType::Group,
                created_at: timestamp,
                group_image_path: None,
                group_image_url: None,
                last_message: Some(ChatMessageSummary {
                    message_id: nostr_sdk::EventId::all_zeros(),
                    mls_group_id: GroupId::from_slice(&[128u8; 32]),
                    author: nostr_sdk::Keys::generate().public_key(),
                    author_display_name: None,
                    content: "Test".to_string(),
                    created_at: msg_timestamp,
                    media_attachment_count: 0,
                }),
                pending_confirmation: false,
                welcomer_pubkey: None,
                unread_count: 0,
            },
        ];

        sort_chat_list(&mut items);

        // Should be sorted by group_id when message timestamps are identical
        // This FAILS without tiebreaker - items stay in wrong order
        assert_eq!(items[0].mls_group_id.as_slice(), &[1u8; 32]);
        assert_eq!(items[1].mls_group_id.as_slice(), &[128u8; 32]);
        assert_eq!(items[2].mls_group_id.as_slice(), &[255u8; 32]);
    }

    #[tokio::test]
    async fn test_sorting_stable_without_messages() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        // Create multiple groups rapidly (same created_at timestamp)
        let mut groups = Vec::new();
        for i in 0..3 {
            let mut config = create_nostr_group_config_data(vec![creator.pubkey]);
            config.name = format!("Group {}", i);
            let group = whitenoise
                .create_group(&creator, vec![member.pubkey], config, None)
                .await
                .unwrap();
            groups.push(group);
        }

        // Fetch chat list multiple times - order should be stable
        let list1 = whitenoise.get_chat_list(&creator).await.unwrap();
        let list2 = whitenoise.get_chat_list(&creator).await.unwrap();

        assert_eq!(list1.len(), 3);
        assert_eq!(list2.len(), 3);
        // Order should be identical across calls
        for i in 0..3 {
            assert_eq!(list1[i].mls_group_id, list2[i].mls_group_id);
        }
    }

    #[tokio::test]
    async fn test_sorting_stable_with_identical_timestamps() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let mut groups = Vec::new();
        for i in 0..3 {
            let mut config = create_nostr_group_config_data(vec![creator.pubkey]);
            config.name = format!("Group {}", i);
            let group = whitenoise
                .create_group(&creator, vec![member.pubkey], config, None)
                .await
                .unwrap();
            groups.push(group);
        }

        // Add messages with identical timestamps to all groups
        let same_timestamp = Timestamp::from(5000);
        for group in &groups {
            let msg = ChatMessage {
                id: format!("{:0>64}", hex::encode(group.mls_group_id.as_slice())),
                author: creator.pubkey,
                content: "Test".to_string(),
                created_at: same_timestamp,
                tags: nostr_sdk::Tags::new(),
                is_reply: false,
                reply_to_id: None,
                is_deleted: false,
                content_tokens: vec![],
                reactions: Default::default(),
                kind: 9,
                media_attachments: vec![],
            };
            AggregatedMessage::insert_message(&msg, &group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();
        }

        // Order should be stable when timestamps are identical
        let list1 = whitenoise.get_chat_list(&creator).await.unwrap();
        let list2 = whitenoise.get_chat_list(&creator).await.unwrap();

        assert_eq!(list1.len(), 3);
        for i in 0..3 {
            assert_eq!(list1[i].mls_group_id, list2[i].mls_group_id);
        }
    }
}

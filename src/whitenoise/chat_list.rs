use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use futures::future::join_all;
use mdk_core::prelude::*;
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr_sdk::PublicKey;
use serde::{Deserialize, Serialize};

use crate::whitenoise::{
    Whitenoise,
    accounts::Account,
    aggregated_message::AggregatedMessage,
    error::Result,
    group_information::{GroupInformation, GroupType},
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

/// Identifies the "other user" in each DM group.
fn identify_dm_participants(
    groups: &[group_types::Group],
    group_info_map: &HashMap<GroupId, GroupInformation>,
    mdk: &MDK<MdkSqliteStorage>,
    account_pubkey: &PublicKey,
) -> Result<HashMap<GroupId, PublicKey>> {
    let mut dm_other_users = HashMap::new();

    for group in groups {
        if let Some(info) = group_info_map.get(&group.mls_group_id)
            && info.group_type == GroupType::DirectMessage
        {
            let members: Vec<PublicKey> =
                mdk.get_members(&group.mls_group_id)?.into_iter().collect();
            if let Some(other_pk) = get_dm_other_user(&members, account_pubkey) {
                dm_other_users.insert(group.mls_group_id.clone(), other_pk);
            }
        }
    }

    Ok(dm_other_users)
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

            Some(ChatListItem {
                mls_group_id: group.mls_group_id.clone(),
                name,
                group_type: group_info.group_type.clone(),
                created_at: group_info.created_at,
                group_image_path,
                group_image_url,
                last_message,
            })
        })
        .collect()
}

/// Sorts chat list items by last activity (most recent first).
/// Groups without messages are sorted by creation date.
fn sort_chat_list(items: &mut [ChatListItem]) {
    items.sort_by(|a, b| {
        let a_time = a
            .last_message
            .as_ref()
            .map(|m| m.created_at)
            .unwrap_or(a.created_at);
        let b_time = b
            .last_message
            .as_ref()
            .map(|m| m.created_at)
            .unwrap_or(b.created_at);
        b_time.cmp(&a_time) // Descending (most recent first)
    });
}

impl Whitenoise {
    /// Retrieves the chat list for an account.
    ///
    /// Returns a list of chat summaries sorted by last activity (most recent first).
    /// Groups without messages are sorted by creation date.
    pub async fn get_chat_list(&self, account: &Account) -> Result<Vec<ChatListItem>> {
        let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
        let groups = mdk.get_groups()?;
        if groups.is_empty() {
            return Ok(Vec::new());
        }

        let group_ids: Vec<GroupId> = groups.iter().map(|g| g.mls_group_id.clone()).collect();

        let group_info_map = self
            .build_group_info_map(account.pubkey, &group_ids)
            .await?;
        let dm_other_users =
            identify_dm_participants(&groups, &group_info_map, &mdk, &account.pubkey)?;
        let last_message_map = self.build_last_message_map(&group_ids).await;
        let pubkeys_to_fetch = collect_pubkeys_to_fetch(&dm_other_users, &last_message_map);
        let users_by_pubkey = self.build_users_by_pubkey(&pubkeys_to_fetch).await;
        let image_paths = self
            .resolve_group_images(account, &groups, &group_info_map)
            .await;

        let mut items = assemble_chat_list_items(
            &groups,
            &group_info_map,
            &dm_other_users,
            &last_message_map,
            &users_by_pubkey,
            &image_paths,
        );
        sort_chat_list(&mut items);

        Ok(items)
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

    async fn build_last_message_map(
        &self,
        group_ids: &[GroupId],
    ) -> HashMap<GroupId, ChatMessageSummary> {
        AggregatedMessage::find_last_by_group_ids(group_ids, &self.database)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.mls_group_id.clone(), s))
            .collect()
    }

    async fn build_users_by_pubkey(&self, pubkeys: &[PublicKey]) -> HashMap<PublicKey, User> {
        User::find_by_pubkeys(pubkeys, &self.database)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|u| (u.pubkey, u))
            .collect()
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
    use nostr_sdk::{Keys, Metadata};

    #[test]
    fn test_resolve_display_name_with_display_name() {
        let user = User {
            id: Some(1),
            pubkey: Keys::generate().public_key(),
            metadata: Metadata::new().display_name("Display").name("Name"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert_eq!(
            resolve_display_name(Some(&user)),
            Some("Display".to_string())
        );
    }

    #[test]
    fn test_resolve_display_name_falls_back_to_name() {
        let user = User {
            id: Some(1),
            pubkey: Keys::generate().public_key(),
            metadata: Metadata::new().name("Name"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert_eq!(resolve_display_name(Some(&user)), Some("Name".to_string()));
    }

    #[test]
    fn test_resolve_display_name_empty_display_name_falls_back() {
        let mut metadata = Metadata::new().name("Name");
        metadata.display_name = Some(String::new()); // Empty display name
        let user = User {
            id: Some(1),
            pubkey: Keys::generate().public_key(),
            metadata,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert_eq!(resolve_display_name(Some(&user)), Some("Name".to_string()));
    }

    #[test]
    fn test_resolve_display_name_none_when_no_metadata() {
        let user = User {
            id: Some(1),
            pubkey: Keys::generate().public_key(),
            metadata: Metadata::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert_eq!(resolve_display_name(Some(&user)), None);
    }

    #[test]
    fn test_resolve_display_name_none_when_no_user() {
        assert_eq!(resolve_display_name(None), None);
    }

    #[test]
    fn test_get_dm_other_user() {
        let account_pk = Keys::generate().public_key();
        let other_pk = Keys::generate().public_key();
        let members = vec![account_pk, other_pk];

        assert_eq!(get_dm_other_user(&members, &account_pk), Some(other_pk));
    }

    #[test]
    fn test_get_dm_other_user_not_found() {
        let account_pk = Keys::generate().public_key();
        let members = vec![account_pk]; // Only account owner

        assert_eq!(get_dm_other_user(&members, &account_pk), None);
    }

    #[test]
    fn test_sort_chat_list_by_last_message() {
        let group_id1 = GroupId::from_slice(&[1; 32]);
        let group_id2 = GroupId::from_slice(&[2; 32]);
        let author = Keys::generate().public_key();

        let older = DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let newer = DateTime::parse_from_rfc3339("2024-01-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let mut items = vec![
            ChatListItem {
                mls_group_id: group_id1.clone(),
                name: Some("Older".to_string()),
                group_type: GroupType::Group,
                created_at: older,
                group_image_path: None,
                group_image_url: None,
                last_message: Some(ChatMessageSummary {
                    mls_group_id: group_id1,
                    author,
                    author_display_name: None,
                    content: "Old message".to_string(),
                    created_at: older,
                    media_attachment_count: 0,
                }),
            },
            ChatListItem {
                mls_group_id: group_id2.clone(),
                name: Some("Newer".to_string()),
                group_type: GroupType::Group,
                created_at: older,
                group_image_path: None,
                group_image_url: None,
                last_message: Some(ChatMessageSummary {
                    mls_group_id: group_id2,
                    author,
                    author_display_name: None,
                    content: "New message".to_string(),
                    created_at: newer,
                    media_attachment_count: 0,
                }),
            },
        ];

        sort_chat_list(&mut items);

        // Newer should be first
        assert_eq!(items[0].name, Some("Newer".to_string()));
        assert_eq!(items[1].name, Some("Older".to_string()));
    }

    #[test]
    fn test_sort_chat_list_no_messages_uses_created_at() {
        let group_id1 = GroupId::from_slice(&[1; 32]);
        let group_id2 = GroupId::from_slice(&[2; 32]);

        let older = DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let newer = DateTime::parse_from_rfc3339("2024-01-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let mut items = vec![
            ChatListItem {
                mls_group_id: group_id1,
                name: Some("Older Group".to_string()),
                group_type: GroupType::Group,
                created_at: older,
                group_image_path: None,
                group_image_url: None,
                last_message: None,
            },
            ChatListItem {
                mls_group_id: group_id2,
                name: Some("Newer Group".to_string()),
                group_type: GroupType::Group,
                created_at: newer,
                group_image_path: None,
                group_image_url: None,
                last_message: None,
            },
        ];

        sort_chat_list(&mut items);

        // Newer created_at should be first
        assert_eq!(items[0].name, Some("Newer Group".to_string()));
        assert_eq!(items[1].name, Some("Older Group".to_string()));
    }

    #[test]
    fn test_sort_chat_list_mixed_with_and_without_messages() {
        let group_id1 = GroupId::from_slice(&[1; 32]);
        let group_id2 = GroupId::from_slice(&[2; 32]);
        let author = Keys::generate().public_key();

        let old_created = DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let message_time = DateTime::parse_from_rfc3339("2024-01-05T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let new_created = DateTime::parse_from_rfc3339("2024-01-10T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let mut items = vec![
            // Old group with a message at day 5
            ChatListItem {
                mls_group_id: group_id1.clone(),
                name: Some("Old with message".to_string()),
                group_type: GroupType::Group,
                created_at: old_created,
                group_image_path: None,
                group_image_url: None,
                last_message: Some(ChatMessageSummary {
                    mls_group_id: group_id1,
                    author,
                    author_display_name: None,
                    content: "Message".to_string(),
                    created_at: message_time,
                    media_attachment_count: 0,
                }),
            },
            // New group with no messages
            ChatListItem {
                mls_group_id: group_id2,
                name: Some("New no message".to_string()),
                group_type: GroupType::Group,
                created_at: new_created,
                group_image_path: None,
                group_image_url: None,
                last_message: None,
            },
        ];

        sort_chat_list(&mut items);

        // New group (created day 10) should be first because its effective time (day 10)
        // is more recent than the message time (day 5)
        assert_eq!(items[0].name, Some("New no message".to_string()));
        assert_eq!(items[1].name, Some("Old with message".to_string()));
    }
}

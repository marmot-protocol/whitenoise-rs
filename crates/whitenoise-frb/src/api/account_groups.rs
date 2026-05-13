use crate::api::wn_session;
use crate::api::{error::ApiError, utils::group_id_to_string};
use flutter_rust_bridge::frb;
use nostr_sdk::prelude::*;
use whitenoise::AccountGroup as WhitenoiseAccountGroup;
use whitenoise::mdk::GroupId;

/// Represents the relationship between an account and an MLS group.
///
/// This struct tracks whether a user has accepted or declined a group invite.
/// When a welcome message is received, groups are auto-joined at the MLS level,
/// but the AccountGroup tracks the user's UI-level confirmation.
///
/// Confirmation states:
/// - `Pending` = auto-joined but awaiting user decision
/// - `Accepted` = user confirmed they want to see the group
/// - `Declined` = user chose to hide the group from UI
#[frb]
#[derive(Debug, Clone)]
pub struct AccountGroup {
    pub id: Option<i64>,
    pub account_pubkey: String,
    pub mls_group_id: String,
    pub user_confirmation: Option<bool>,
    pub welcomer_pubkey: Option<String>,
    /// The last message the user has read in this group (hex EventId).
    /// Used to compute unread counts.
    pub last_read_message_id: Option<String>,
    /// Pin order for chat list sorting.
    /// - `None` = not pinned (appears after pinned chats)
    /// - `Some(n)` = pinned, lower values appear first
    pub pin_order: Option<i64>,
    pub archived_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<WhitenoiseAccountGroup> for AccountGroup {
    fn from(ag: WhitenoiseAccountGroup) -> Self {
        Self {
            id: ag.id,
            account_pubkey: ag.account_pubkey.to_hex(),
            mls_group_id: group_id_to_string(&ag.mls_group_id),
            user_confirmation: ag.user_confirmation,
            welcomer_pubkey: ag.welcomer_pubkey.map(|pk| pk.to_hex()),
            last_read_message_id: ag.last_read_message_id.map(|id| id.to_hex()),
            pin_order: ag.pin_order,
            archived_at: ag.archived_at.map(|dt| dt.timestamp_millis()),
            created_at: ag.created_at.timestamp_millis(),
            updated_at: ag.updated_at.timestamp_millis(),
        }
    }
}

impl From<&WhitenoiseAccountGroup> for AccountGroup {
    fn from(ag: &WhitenoiseAccountGroup) -> Self {
        Self {
            id: ag.id,
            account_pubkey: ag.account_pubkey.to_hex(),
            mls_group_id: group_id_to_string(&ag.mls_group_id),
            user_confirmation: ag.user_confirmation,
            welcomer_pubkey: ag.welcomer_pubkey.map(|pk| pk.to_hex()),
            last_read_message_id: ag.last_read_message_id.map(|id| id.to_hex()),
            pin_order: ag.pin_order,
            archived_at: ag.archived_at.map(|dt| dt.timestamp_millis()),
            created_at: ag.created_at.timestamp_millis(),
            updated_at: ag.updated_at.timestamp_millis(),
        }
    }
}

/// Accepts a group invite by setting user_confirmation to true.
/// The group will remain visible in the UI.
#[frb]
pub async fn accept_account_group(
    account_pubkey: String,
    mls_group_id: String,
) -> Result<AccountGroup, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let group_id_bytes = ::hex::decode(&mls_group_id)?;
    let group_id = GroupId::from_slice(&group_id_bytes);
    let session = wn_session(&pubkey)?;
    let updated = session.membership().for_group(&group_id).accept().await?;
    Ok(updated.into())
}

/// Declines a group invite by setting user_confirmation to false.
/// The group will be hidden from the UI but remains in MLS.
#[frb]
pub async fn decline_account_group(
    account_pubkey: String,
    mls_group_id: String,
) -> Result<AccountGroup, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let group_id_bytes = ::hex::decode(&mls_group_id)?;
    let group_id = GroupId::from_slice(&group_id_bytes);
    let session = wn_session(&pubkey)?;
    let updated = session.membership().for_group(&group_id).decline().await?;
    Ok(updated.into())
}

#[frb]
pub async fn get_account_group(
    account_pubkey: String,
    mls_group_id: String,
) -> Result<AccountGroup, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let group_id_bytes = ::hex::decode(&mls_group_id)?;
    let group_id = GroupId::from_slice(&group_id_bytes);
    let session = wn_session(&pubkey)?;
    let (ag, _) = session
        .membership()
        .for_group(&group_id)
        .get_or_create(None)
        .await?;
    Ok(ag.into())
}

#[frb]
pub async fn get_dm_group_with_peer(
    account_pubkey: String,
    peer_pubkey: String,
) -> Result<Option<String>, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let peer = PublicKey::parse(&peer_pubkey)?;
    let session = wn_session(&pubkey)?;
    let group_id = session.membership().dm_group_with_peer(&peer).await?;
    Ok(group_id.map(|id| group_id_to_string(&id)))
}

#[frb]
pub async fn archive_chat(account_pubkey: String, mls_group_id: String) -> Result<(), ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let group_id_bytes = ::hex::decode(&mls_group_id)?;
    let group_id = GroupId::from_slice(&group_id_bytes);
    let session = wn_session(&pubkey)?;
    session.membership().for_group(&group_id).archive().await?;
    Ok(())
}

#[frb]
pub async fn unarchive_chat(account_pubkey: String, mls_group_id: String) -> Result<(), ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let group_id_bytes = ::hex::decode(&mls_group_id)?;
    let group_id = GroupId::from_slice(&group_id_bytes);
    let session = wn_session(&pubkey)?;
    session
        .membership()
        .for_group(&group_id)
        .unarchive()
        .await?;
    Ok(())
}

/// Marks a message as read for the given account.
///
/// Updates the `last_read_message_id` for the account-group pair containing
/// the specified message. This is used to compute unread counts in the chat list.
#[frb]
pub async fn mark_message_read(
    account_pubkey: String,
    message_id: String,
) -> Result<AccountGroup, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let event_id = EventId::from_hex(&message_id)?;
    let session = wn_session(&pubkey)?;
    let updated = session.membership().mark_message_read(&event_id).await?;
    Ok(updated.into())
}

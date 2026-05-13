use crate::api::{
    default_relay_urls_parsed, error::ApiError, group_id_from_string, group_id_to_string,
};
use crate::api::{wn, wn_session};
use chrono::{DateTime, Utc};
use flutter_rust_bridge::frb;
use nostr_sdk::prelude::*;
use whitenoise::mdk::{Group as WhitenoiseGroup, GroupState as WhitenoiseGroupState};
use whitenoise::mdk::{NostrGroupConfigData, NostrGroupDataUpdate};
use whitenoise::whitenoise::groups::RequiredProposal as WhitenoiseRequiredProposal;
use whitenoise::{
    GroupInformation as WhitenoiseGroupInformation, GroupType as WhitenoiseGroupType,
    GroupWithInfoAndMembership as WhitenoiseGroupWithInfoAndMembership,
};
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct Group {
    pub mls_group_id: String,
    pub nostr_group_id: String,
    pub name: String,
    pub description: String,
    pub image_hash: Option<[u8; 32]>,
    pub image_key: Option<[u8; 32]>,
    pub admin_pubkeys: Vec<String>,
    pub last_message_id: Option<String>,
    pub last_message_at: Option<DateTime<Utc>>,
    pub epoch: u64,
    pub state: GroupState,
}

impl From<WhitenoiseGroup> for Group {
    fn from(group: WhitenoiseGroup) -> Self {
        Self {
            mls_group_id: group_id_to_string(&group.mls_group_id),
            nostr_group_id: hex::encode(group.nostr_group_id),
            name: group.name,
            description: group.description,
            image_hash: group.image_hash,
            image_key: group.image_key.map(|s| *s),
            admin_pubkeys: group.admin_pubkeys.iter().map(|pk| pk.to_hex()).collect(),
            last_message_id: group.last_message_id.map(|id| id.to_hex()),
            last_message_at: group.last_message_at.map(|ts| {
                let ts = i64::try_from(ts.as_secs()).unwrap_or(0);
                DateTime::from_timestamp(ts, 0)
                    .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap())
            }),
            epoch: group.epoch,
            state: group.state.into(),
        }
    }
}

#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct FlutterGroupDataUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
    pub relays: Option<Vec<String>>,
    pub admins: Option<Vec<String>>,
    pub image_key: Option<[u8; 32]>,
    pub image_hash: Option<[u8; 32]>,
    pub image_nonce: Option<[u8; 12]>,
}

impl From<FlutterGroupDataUpdate> for NostrGroupDataUpdate {
    fn from(group_data: FlutterGroupDataUpdate) -> Self {
        Self {
            name: group_data.name,
            description: group_data.description,
            image_key: group_data.image_key.map(Some),
            image_hash: group_data.image_hash.map(Some),
            image_nonce: group_data.image_nonce.map(Some),
            image_upload_key: None,
            nostr_group_id: None,
            disappearing_message_secs: None,
            relays: group_data.relays.map(|relays| {
                relays
                    .into_iter()
                    .filter_map(|r| RelayUrl::parse(&r).ok())
                    .collect()
            }),
            admins: group_data.admins.map(|admins| {
                admins
                    .into_iter()
                    .filter_map(|a| PublicKey::parse(&a).ok())
                    .collect()
            }),
        }
    }
}

impl Group {
    #[frb]
    pub async fn group_type(&self, account_pubkey: String) -> Result<GroupType, ApiError> {
        let whitenoise = wn()?;
        let mls_group_id = group_id_from_string(&self.mls_group_id)?;
        let parsed_pubkey = PublicKey::parse(&account_pubkey)?;
        let group_information = whitenoise
            .get_group_information_by_mls_group_id(parsed_pubkey, &mls_group_id)
            .await?;
        Ok(group_information.group_type.into())
    }

    #[frb]
    pub async fn is_direct_message_type(&self, account_pubkey: String) -> Result<bool, ApiError> {
        let whitenoise = wn()?;
        let mls_group_id = group_id_from_string(&self.mls_group_id)?;
        let parsed_pubkey = PublicKey::parse(&account_pubkey)?;
        let group_information = whitenoise
            .get_group_information_by_mls_group_id(parsed_pubkey, &mls_group_id)
            .await?;
        Ok(group_information.group_type == WhitenoiseGroupType::DirectMessage)
    }

    #[frb]
    pub async fn is_group_type(&self, account_pubkey: String) -> Result<bool, ApiError> {
        let whitenoise = wn()?;
        let mls_group_id = group_id_from_string(&self.mls_group_id)?;
        let parsed_pubkey = PublicKey::parse(&account_pubkey)?;
        let group_information = whitenoise
            .get_group_information_by_mls_group_id(parsed_pubkey, &mls_group_id)
            .await?;
        Ok(group_information.group_type == WhitenoiseGroupType::Group)
    }

    #[frb]
    pub async fn update_group_data(
        &self,
        account_pubkey: String,
        group_data: FlutterGroupDataUpdate,
    ) -> Result<(), ApiError> {
        let mls_group_id = group_id_from_string(&self.mls_group_id)?;
        let parsed_pubkey = PublicKey::parse(&account_pubkey)?;
        let session = wn_session(&parsed_pubkey)?;
        session
            .groups()
            .update_group_data(&mls_group_id, group_data.into())
            .await
            .map_err(ApiError::from)
    }
}

// Define our own GroupState enum that can be used by Dart
#[frb]
#[derive(Debug, Clone)]
pub enum GroupState {
    Active,
    Inactive,
    Pending,
}

// Implement conversion from the whitenoise crate's GroupState to our GroupState
impl From<WhitenoiseGroupState> for GroupState {
    fn from(whitenoise_state: WhitenoiseGroupState) -> Self {
        match whitenoise_state {
            WhitenoiseGroupState::Active => GroupState::Active,
            WhitenoiseGroupState::Inactive => GroupState::Inactive,
            WhitenoiseGroupState::Pending => GroupState::Pending,
        }
    }
}

// Define our own GroupType enum that can be used by Dart
#[frb]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupType {
    DirectMessage,
    Group,
}

// Implement conversion from the whitenoise crate's GroupType to our GroupType
impl From<WhitenoiseGroupType> for GroupType {
    fn from(whitenoise_type: WhitenoiseGroupType) -> Self {
        match whitenoise_type {
            WhitenoiseGroupType::DirectMessage => GroupType::DirectMessage,
            WhitenoiseGroupType::Group => GroupType::Group,
        }
    }
}

#[frb]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequiredProposal {
    SelfRemove,
    Unknown,
}

impl From<WhitenoiseRequiredProposal> for RequiredProposal {
    fn from(required_proposal: WhitenoiseRequiredProposal) -> Self {
        match required_proposal {
            WhitenoiseRequiredProposal::SelfRemove => RequiredProposal::SelfRemove,
            WhitenoiseRequiredProposal::Unknown => RequiredProposal::Unknown,
        }
    }
}

// Define our own GroupInformation struct that can be used by Dart
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct GroupInformation {
    pub mls_group_id: String,
    pub group_type: GroupType,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// Implement conversion from the whitenoise crate's GroupInformation to our GroupInformation
impl From<WhitenoiseGroupInformation> for GroupInformation {
    fn from(whitenoise_info: WhitenoiseGroupInformation) -> Self {
        Self {
            mls_group_id: group_id_to_string(&whitenoise_info.mls_group_id),
            group_type: whitenoise_info.group_type.into(),
            created_at: whitenoise_info.created_at,
            updated_at: whitenoise_info.updated_at,
        }
    }
}

#[frb]
pub async fn active_groups(pubkey: String) -> Result<Vec<Group>, ApiError> {
    let pubkey = PublicKey::parse(&pubkey)?;
    let session = wn_session(&pubkey)?;
    let groups = session.groups().all(true)?;
    Ok(groups.into_iter().map(|g| g.into()).collect())
}

#[frb]
pub async fn group_members(pubkey: String, group_id: String) -> Result<Vec<String>, ApiError> {
    let pubkey = PublicKey::parse(&pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let session = wn_session(&pubkey)?;
    let members = session.groups().members(&group_id)?;
    Ok(members.into_iter().map(|m| m.to_hex()).collect())
}

#[frb]
pub async fn group_admins(pubkey: String, group_id: String) -> Result<Vec<String>, ApiError> {
    let pubkey = PublicKey::parse(&pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let session = wn_session(&pubkey)?;
    let admins = session.groups().admins(&group_id)?;
    Ok(admins.into_iter().map(|a| a.to_hex()).collect())
}

#[frb]
pub async fn create_group(
    creator_pubkey: String,
    member_pubkeys: Vec<String>,
    admin_pubkeys: Vec<String>,
    group_name: String,
    group_description: String,
    group_type: GroupType,
) -> Result<Group, ApiError> {
    let whitenoise_group_type = match group_type {
        GroupType::DirectMessage => WhitenoiseGroupType::DirectMessage,
        GroupType::Group => WhitenoiseGroupType::Group,
    };

    let creator_pubkey = PublicKey::parse(&creator_pubkey)?;
    let session = wn_session(&creator_pubkey)?;

    let admin_pubkeys = admin_pubkeys
        .into_iter()
        .map(|pk| PublicKey::parse(&pk))
        .collect::<Result<Vec<_>, _>>()?;

    let nostr_group_config = NostrGroupConfigData {
        name: group_name,
        description: group_description,
        image_key: None,
        image_hash: None,
        image_nonce: None,
        relays: default_relay_urls_parsed()?,
        admins: admin_pubkeys,
        disappearing_message_secs: None,
    };

    let member_pubkeys = member_pubkeys
        .into_iter()
        .map(|pk| PublicKey::parse(&pk))
        .collect::<Result<Vec<_>, _>>()?;

    let group = session
        .groups()
        .create_group(
            member_pubkeys,
            nostr_group_config,
            Some(whitenoise_group_type),
        )
        .await?;
    Ok(group.into())
}

#[frb]
pub async fn add_members_to_group(
    pubkey: String,
    group_id: String,
    member_pubkeys: Vec<String>,
) -> Result<(), ApiError> {
    let pubkey = PublicKey::parse(&pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let session = wn_session(&pubkey)?;
    let member_pubkeys = member_pubkeys
        .into_iter()
        .map(|pk| PublicKey::parse(&pk))
        .collect::<Result<Vec<_>, _>>()?;
    session
        .groups()
        .add_members(&group_id, member_pubkeys)
        .await
        .map_err(ApiError::from)
}

#[frb]
pub async fn remove_members_from_group(
    pubkey: String,
    group_id: String,
    member_pubkeys: Vec<String>,
) -> Result<(), ApiError> {
    let pubkey = PublicKey::parse(&pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let session = wn_session(&pubkey)?;
    let member_pubkeys = member_pubkeys
        .into_iter()
        .map(|pk| PublicKey::parse(&pk))
        .collect::<Result<Vec<_>, _>>()?;
    session
        .groups()
        .remove_members(&group_id, member_pubkeys)
        .await
        .map_err(ApiError::from)
}

/// Clears all messages in a group's chat for this account.
///
/// The group stays in the chat list and the account remains an MLS member.
/// Local-only: nothing is published to relays.
#[frb]
pub async fn clear_chat(account_pubkey: String, group_id: String) -> Result<(), ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let session = wn_session(&pubkey)?;
    session
        .membership()
        .for_group(&group_id)
        .clear_chat()
        .await?;
    Ok(())
}

/// Removes a group from this account's chat list and deletes its local data.
///
/// Local-only: nothing is published to relays, so the account stays an MLS
/// member and other members keep encrypting to it. Use [`leave_and_delete_group`]
/// to exit the group entirely.
#[frb]
pub async fn delete_chat(account_pubkey: String, group_id: String) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;
    whitenoise.delete_chat(&account, &group_id).await?;
    Ok(())
}

/// Leaves the MLS group, then deletes all local chat data for this account.
///
/// Publishes a leave proposal to the group's relays so other members stop
/// encrypting to this account. If the leave fails, local data is untouched.
#[frb]
pub async fn leave_and_delete_group(
    account_pubkey: String,
    group_id: String,
) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;
    let session = wn_session(&pubkey)?;
    session.groups().leave(&group_id).await?;
    whitenoise.delete_chat(&account, &group_id).await?;
    Ok(())
}

#[frb]
pub async fn leave_group(pubkey: String, group_id: String) -> Result<(), ApiError> {
    let pubkey = PublicKey::parse(&pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let session = wn_session(&pubkey)?;
    session
        .groups()
        .leave(&group_id)
        .await
        .map_err(ApiError::from)
}

#[frb]
pub async fn get_group(account_pubkey: String, group_id: String) -> Result<Group, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let group = session.groups().get(&group_id)?;
    Ok(group.into())
}

#[frb]
pub async fn group_required_proposals(
    account_pubkey: String,
    group_id: String,
) -> Result<Vec<RequiredProposal>, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;
    let group_id = group_id_from_string(&group_id)?;
    let required_proposals = whitenoise
        .group_required_proposals(&account, &group_id)
        .await?;
    Ok(required_proposals
        .into_iter()
        .map(RequiredProposal::from)
        .collect())
}

#[frb]
pub async fn get_group_information(
    account_pubkey: String,
    group_id: String,
) -> Result<GroupInformation, ApiError> {
    let whitenoise = wn()?;
    let group_id = group_id_from_string(&group_id)?;
    let parsed_pubkey = PublicKey::parse(&account_pubkey)?;
    Ok(whitenoise
        .get_group_information_by_mls_group_id(parsed_pubkey, &group_id)
        .await?
        .into())
}

#[frb]
pub async fn get_groups_informations(
    account_pubkey: String,
    group_ids: Vec<String>,
) -> Result<Vec<GroupInformation>, ApiError> {
    let whitenoise = wn()?;
    let group_ids = group_ids
        .into_iter()
        .map(|id| group_id_from_string(&id))
        .collect::<Result<Vec<_>, _>>()?;
    let parsed_pubkey = PublicKey::parse(&account_pubkey)?;
    Ok(whitenoise
        .get_group_information_by_mls_group_ids(parsed_pubkey, &group_ids)
        .await?
        .into_iter()
        .map(|info| info.into())
        .collect())
}

/// A group paired with its metadata and account membership record.
/// Callers can filter on `info.group_type` to separate DMs from regular groups.
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct GroupWithInfoAndMembership {
    pub group: Group,
    pub info: GroupInformation,
    pub membership: crate::api::account_groups::AccountGroup,
}

impl From<WhitenoiseGroupWithInfoAndMembership> for GroupWithInfoAndMembership {
    fn from(w: WhitenoiseGroupWithInfoAndMembership) -> Self {
        Self {
            group: w.group.into(),
            info: w.info.into(),
            membership: (&w.membership).into(),
        }
    }
}

#[frb]
pub async fn visible_groups_with_info(
    account_pubkey: String,
) -> Result<Vec<GroupWithInfoAndMembership>, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&pubkey)?;
    let groups = session.groups().visible_with_info().await?;
    Ok(groups.into_iter().map(|g| g.into()).collect())
}

// Result structure for upload_group_image
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct UploadGroupImageResult {
    pub encrypted_hash: [u8; 32],
    pub image_key: [u8; 32],
    pub image_nonce: [u8; 12],
}

#[frb]
pub async fn upload_group_image(
    account_pubkey: String,
    group_id: String,
    file_path: String,
    server_url: String,
) -> Result<UploadGroupImageResult, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let session = wn_session(&pubkey)?;
    let server = Url::parse(&server_url)?;

    let (encrypted_hash, image_key, image_nonce) = session
        .groups()
        .media()
        .upload_group_image(&group_id, &file_path, Some(server), None)
        .await?;

    Ok(UploadGroupImageResult {
        encrypted_hash,
        image_key,
        image_nonce,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_proposal_from_whitenoise_required_proposal() {
        assert_eq!(
            RequiredProposal::from(WhitenoiseRequiredProposal::SelfRemove),
            RequiredProposal::SelfRemove,
        );
        assert_eq!(
            RequiredProposal::from(WhitenoiseRequiredProposal::Unknown),
            RequiredProposal::Unknown,
        );
    }
}

#[frb]
pub async fn get_group_image_path(
    account_pubkey: String,
    group_id: String,
) -> Result<Option<String>, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let session = wn_session(&pubkey)?;
    let path = session
        .groups()
        .media()
        .get_group_image_path(&group_id)
        .await?;
    Ok(path.map(|p| p.to_string_lossy().to_string()))
}

/// Public information about a leaf node in the ratchet tree
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct LeafNodeInfo {
    /// The leaf index in the ratchet tree
    pub index: u32,
    /// The member's public HPKE encryption key (hex-encoded)
    pub encryption_key: String,
    /// The member's public signature key (hex-encoded)
    pub signature_key: String,
    /// The member's credential identity (hex-encoded, typically a Nostr public key)
    pub credential_identity: String,
}

/// Public information about the ratchet tree of an MLS group
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct RatchetTreeInfo {
    /// SHA-256 fingerprint of the TLS-serialized ratchet tree (hex-encoded)
    pub tree_hash: String,
    /// The full ratchet tree serialized via TLS encoding (hex-encoded)
    pub serialized_tree: String,
    /// Leaf nodes with their indices and public keys
    pub leaf_nodes: Vec<LeafNodeInfo>,
}

#[frb]
pub async fn get_ratchet_tree_info(
    account_pubkey: String,
    group_id: String,
) -> Result<RatchetTreeInfo, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;
    let info = whitenoise.ratchet_tree_info(&account, &group_id)?;
    Ok(RatchetTreeInfo {
        tree_hash: info.tree_hash,
        serialized_tree: info.serialized_tree,
        leaf_nodes: info
            .leaf_nodes
            .into_iter()
            .map(|l| LeafNodeInfo {
                index: l.index,
                encryption_key: l.encryption_key,
                signature_key: l.signature_key,
                credential_identity: l.credential_identity,
            })
            .collect(),
    })
}

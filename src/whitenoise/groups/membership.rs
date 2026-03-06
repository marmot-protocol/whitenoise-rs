use mdk_core::prelude::group_types;

use crate::whitenoise::accounts_groups::AccountGroup;
use crate::whitenoise::group_information::GroupInformation;

/// An MLS group paired with its account-specific membership data.
///
/// This struct combines an MLS group with its `AccountGroup` record (the join table
/// between accounts and groups). The `AccountGroup` tracks account-specific state
/// such as user confirmation status (pending, accepted, or declined).
#[derive(Debug, Clone, serde::Serialize)]
pub struct GroupWithMembership {
    /// The MLS group data from MDK
    pub group: group_types::Group,
    /// The account-group relationship (join table record)
    pub membership: AccountGroup,
}

impl GroupWithMembership {
    /// Returns true if this group is pending user confirmation.
    pub fn is_pending(&self) -> bool {
        self.membership.is_pending()
    }

    /// Returns true if this group has been accepted by the user.
    pub fn is_accepted(&self) -> bool {
        self.membership.is_accepted()
    }
}

/// An MLS group paired with its group information and account-specific membership data.
///
/// Extends [`GroupWithMembership`] with [`GroupInformation`], which carries the
/// [`crate::whitenoise::group_information::GroupType`] (regular group vs. direct message)
/// and timestamps. Callers can use `info.group_type` to filter without making additional
/// per-group queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GroupWithInfoAndMembership {
    /// The MLS group data from MDK
    pub group: group_types::Group,
    /// Group metadata stored in the whitenoise database (type, timestamps)
    pub info: GroupInformation,
    /// The account-group relationship (join table record)
    pub membership: AccountGroup,
}

impl GroupWithInfoAndMembership {
    /// Returns true if this group is pending user confirmation.
    pub fn is_pending(&self) -> bool {
        self.membership.is_pending()
    }

    /// Returns true if this group has been accepted by the user.
    pub fn is_accepted(&self) -> bool {
        self.membership.is_accepted()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use chrono::Utc;
    use mdk_core::prelude::GroupId;
    use mdk_core::prelude::group_types;
    use nostr_sdk::prelude::*;

    use crate::whitenoise::accounts_groups::AccountGroup;
    use crate::whitenoise::group_information::{GroupInformation, GroupType};

    use super::{GroupWithInfoAndMembership, GroupWithMembership};

    fn minimal_group(mls_group_id: GroupId) -> group_types::Group {
        group_types::Group {
            mls_group_id,
            nostr_group_id: [0u8; 32],
            name: String::new(),
            description: String::new(),
            image_hash: None,
            image_key: None,
            image_nonce: None,
            admin_pubkeys: BTreeSet::new(),
            last_message_id: None,
            last_message_at: None,
            last_message_processed_at: None,
            epoch: 0,
            state: group_types::GroupState::Active,
            self_update_state: group_types::SelfUpdateState::Required,
        }
    }

    fn minimal_account_group(
        account_pubkey: PublicKey,
        mls_group_id: GroupId,
        user_confirmation: Option<bool>,
    ) -> AccountGroup {
        let now = Utc::now();
        AccountGroup {
            id: None,
            account_pubkey,
            mls_group_id,
            user_confirmation,
            welcomer_pubkey: None,
            last_read_message_id: None,
            pin_order: None,
            dm_peer_pubkey: None,
            archived_at: None,
            removed_at: None,
            muted_until: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn group_with_membership_is_pending_and_is_accepted_delegate() {
        let keys = Keys::generate();
        let pk = keys.public_key();
        let gid = GroupId::from_slice(&[7u8; 32]);
        let mls_group = minimal_group(gid.clone());

        let pending = GroupWithMembership {
            group: mls_group.clone(),
            membership: minimal_account_group(pk, gid.clone(), None),
        };
        assert!(pending.is_pending());
        assert!(!pending.is_accepted());

        let accepted = GroupWithMembership {
            group: mls_group,
            membership: minimal_account_group(pk, gid, Some(true)),
        };
        assert!(!accepted.is_pending());
        assert!(accepted.is_accepted());
    }

    #[test]
    fn group_with_info_and_membership_is_pending_and_is_accepted_delegate() {
        let keys = Keys::generate();
        let pk = keys.public_key();
        let gid = GroupId::from_slice(&[8u8; 32]);
        let mls_group = minimal_group(gid.clone());
        let now = Utc::now();
        let info = GroupInformation {
            id: None,
            mls_group_id: gid.clone(),
            group_type: GroupType::Group,
            created_at: now,
            updated_at: now,
        };

        let pending = GroupWithInfoAndMembership {
            group: mls_group,
            info,
            membership: minimal_account_group(pk, gid, None),
        };
        assert!(pending.is_pending());
        assert!(!pending.is_accepted());
    }
}

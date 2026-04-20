//! Group read operations scoped to an [`AccountSession`].

use std::collections::BTreeSet;
use std::collections::HashMap;

use mdk_core::prelude::*;
use nostr_sdk::{PublicKey, RelayUrl};

use super::AccountSession;
use crate::whitenoise::accounts_groups::AccountGroup;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::group_information::GroupInformation;
use crate::whitenoise::groups::{GroupWithInfoAndMembership, GroupWithMembership};

/// View over [`AccountSession`] for group read operations.
///
/// Obtain via [`AccountSession::groups`].
pub struct GroupOps<'a> {
    session: &'a AccountSession,
}

impl<'a> GroupOps<'a> {
    pub(super) fn new(session: &'a AccountSession) -> Self {
        Self { session }
    }

    /// Return all groups for this account.
    ///
    /// When `active_filter` is `true`, only groups in `Active` state are
    /// returned. When `false`, all groups (including inactive) are included.
    pub fn all(&self, active_filter: bool) -> Result<Vec<group_types::Group>> {
        let groups: Vec<group_types::Group> = self
            .session
            .mdk
            .get_groups()
            .map_err(WhitenoiseError::from)?
            .into_iter()
            .filter(|group| !active_filter || group.state == group_types::GroupState::Active)
            .collect();

        Ok(groups)
    }

    /// Return visible groups for this account (pending + accepted + removed,
    /// excluding declined).
    ///
    /// `AccountGroup` is the source of truth for visibility:
    /// - **Pending** (`user_confirmation = NULL`) — invited, not yet accepted
    /// - **Accepted** (`user_confirmation = true`) — active member
    /// - **Removed** (`user_confirmation = true`, `removed_at IS NOT NULL`) —
    ///   kicked by admin; stays visible until archived
    /// - **Declined** (`user_confirmation = false`) — hidden, never shown
    ///
    /// All MDK groups (including inactive) are fetched so that removed groups,
    /// which MDK marks as `Inactive`, are still paired with their
    /// `AccountGroup` records.
    pub async fn visible(&self) -> Result<Vec<GroupWithMembership>> {
        let all_groups = self.all(false)?;

        let visible_account_groups = AccountGroup::find_visible_for_account(
            &self.session.account_pubkey,
            &self.session.database,
        )
        .await?;

        let memberships_by_id: HashMap<_, _> = visible_account_groups
            .into_iter()
            .map(|ag| (ag.mls_group_id.clone(), ag))
            .collect();

        Ok(all_groups
            .into_iter()
            .filter_map(|group| {
                let membership = memberships_by_id.get(&group.mls_group_id)?.clone();
                if group.state == group_types::GroupState::Active || membership.is_removed() {
                    Some(GroupWithMembership { group, membership })
                } else {
                    None
                }
            })
            .collect())
    }

    /// Return visible groups paired with their [`GroupInformation`].
    ///
    /// Eliminates the N+1 pattern of calling [`Self::visible`] then fetching
    /// info individually. Groups with no `group_information` row are excluded.
    pub async fn visible_with_info(&self) -> Result<Vec<GroupWithInfoAndMembership>> {
        let visible = self.visible().await?;

        if visible.is_empty() {
            return Ok(Vec::new());
        }

        let group_ids: Vec<_> = visible
            .iter()
            .map(|gwm| gwm.group.mls_group_id.clone())
            .collect();
        let info_list =
            GroupInformation::find_by_mls_group_ids(&group_ids, &self.session.database).await?;
        let info_by_id: HashMap<_, _> = info_list
            .into_iter()
            .map(|gi| (gi.mls_group_id.clone(), gi))
            .collect();

        let result = visible
            .into_iter()
            .filter_map(|gwm| {
                let info = info_by_id.get(&gwm.group.mls_group_id)?.clone();
                Some(GroupWithInfoAndMembership {
                    group: gwm.group,
                    info,
                    membership: gwm.membership,
                })
            })
            .collect();

        Ok(result)
    }

    /// Retrieve a single group by its MLS group ID.
    pub fn get(&self, group_id: &GroupId) -> Result<group_types::Group> {
        self.session
            .mdk
            .get_group(group_id)
            .map_err(WhitenoiseError::from)?
            .ok_or(WhitenoiseError::GroupNotFound)
    }

    /// Return the members of a group.
    pub fn members(&self, group_id: &GroupId) -> Result<Vec<PublicKey>> {
        Ok(self
            .session
            .mdk
            .get_members(group_id)
            .map_err(WhitenoiseError::from)?
            .into_iter()
            .collect::<Vec<PublicKey>>())
    }

    /// Return the relay URLs for a group.
    pub fn relays(&self, group_id: &GroupId) -> Result<BTreeSet<RelayUrl>> {
        self.session
            .mdk
            .get_relays(group_id)
            .map_err(WhitenoiseError::from)
    }

    /// Return the admin public keys for a group.
    pub fn admins(&self, group_id: &GroupId) -> Result<Vec<PublicKey>> {
        Ok(self
            .session
            .mdk
            .get_group(group_id)
            .map_err(WhitenoiseError::from)?
            .ok_or(WhitenoiseError::GroupNotFound)?
            .admin_pubkeys
            .into_iter()
            .collect::<Vec<PublicKey>>())
    }
}

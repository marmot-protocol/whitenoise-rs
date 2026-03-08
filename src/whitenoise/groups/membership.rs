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

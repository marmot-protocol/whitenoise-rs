use mdk_core::prelude::group_types;

use crate::whitenoise::accounts_groups::AccountGroup;

/// An MLS group paired with its account-specific membership data.
///
/// This struct combines an MLS group with its `AccountGroup` record (the join table
/// between accounts and groups). The `AccountGroup` tracks account-specific state
/// such as user confirmation status (pending, accepted, or declined).
#[derive(Debug, Clone)]
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

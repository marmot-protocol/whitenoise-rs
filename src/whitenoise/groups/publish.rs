use mdk_core::prelude::*;
use mdk_sqlite_storage::MdkSqliteStorage;

use crate::whitenoise::{
    Whitenoise,
    error::{Result, WhitenoiseError},
};

impl Whitenoise {
    /// Ensures that group relays are available for publishing evolution events.
    /// Returns the validated relay URLs.
    ///
    /// # Arguments
    /// * `mdk` - The NostrMls instance to get relays from
    /// * `group_id` - The ID of the group
    ///
    /// # Returns
    /// * `Ok(Vec<nostr_sdk::RelayUrl>)` - Vector of relay URLs
    /// * `Err(WhitenoiseError::GroupMissingRelays)` - If no relays are configured
    pub(crate) fn ensure_group_relays(
        mdk: &MDK<MdkSqliteStorage>,
        group_id: &GroupId,
    ) -> Result<Vec<nostr_sdk::RelayUrl>> {
        let group_relays = mdk.get_relays(group_id)?;

        if group_relays.is_empty() {
            return Err(WhitenoiseError::GroupMissingRelays);
        }

        Ok(group_relays.into_iter().collect())
    }
}

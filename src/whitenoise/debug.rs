use mdk_core::prelude::RatchetTreeInfo;
use mdk_storage_traits::GroupId;

use super::Whitenoise;
use super::accounts::Account;
use super::error::{Result, WhitenoiseError};

impl Whitenoise {
    /// Returns public information about the ratchet tree of an MLS group.
    ///
    /// Exposes the MLS ratchet tree structure for a given group. The returned
    /// data contains only public information (encryption keys, signature keys,
    /// tree structure) — no secrets.
    ///
    /// # Arguments
    ///
    /// * `account` - The account that is a member of the group
    /// * `group_id` - The MLS group ID to inspect
    ///
    /// # Returns
    ///
    /// A [`RatchetTreeInfo`] containing the tree hash, serialized tree, and leaf nodes.
    pub fn ratchet_tree_info(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<RatchetTreeInfo> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        mdk.get_ratchet_tree_info(group_id)
            .map_err(WhitenoiseError::from)
    }

    /// Returns the current relay-control snapshot as pretty-printed JSON.
    ///
    /// This is a debug helper for inspecting live relay-plane state without
    /// having to query internal structures manually.
    pub async fn debug_relay_control_state(&self) -> Result<String> {
        let snapshot = self.get_relay_control_state().await;
        serde_json::to_string_pretty(&snapshot).map_err(WhitenoiseError::from)
    }
}

#[cfg(test)]
mod tests {
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    #[tokio::test]
    async fn debug_relay_control_state_returns_json_object() {
        let (wn, _data_dir, _logs_dir) = create_mock_whitenoise().await;
        let json = wn.debug_relay_control_state().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert!(parsed.is_object());
        assert!(parsed.get("discovery").is_some());
        assert!(parsed.get("account_inbox").is_some());
        assert!(parsed.get("group").is_some());
    }
}

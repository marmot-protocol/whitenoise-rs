use super::Whitenoise;
use super::accounts::Account;
use super::error::{Result, WhitenoiseError};
use crate::marmot::{GroupForensics, GroupId};

impl Whitenoise {
    /// Returns public support diagnostics for a Marmot group.
    ///
    /// The returned data is redacted with a stable account/group-local salt so
    /// support tooling can correlate records without exposing raw group,
    /// message, or payload identifiers.
    ///
    /// # Arguments
    ///
    /// * `account` - The account that is a member of the group
    /// * `group_id` - The Marmot group ID to inspect
    ///
    /// # Returns
    ///
    /// A [`GroupForensics`] value containing redacted group state, retained
    /// message diagnostics, snapshots, and warnings.
    pub async fn group_forensics(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<GroupForensics> {
        let session = self.require_session(&account.pubkey)?;
        let marmot_session = session.marmot.clone().ok_or(
            super::error::WhitenoiseError::MarmotSessionUnavailable(account.pubkey),
        )?;
        let marmot_group_id = cgka_traits::types::GroupId::new(group_id.as_slice().to_vec());
        let marmot_session = marmot_session.lock().await;

        marmot_session.group_forensics(&marmot_group_id)
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
    use cgka_traits::app_components::{
        AppComponentData, NOSTR_ROUTING_COMPONENT_ID, NostrRoutingV1, encode_nostr_routing_v1,
    };
    use cgka_traits::engine::CreateGroupRequest;
    use cgka_traits::types::MessageId;

    use crate::marmot::GroupId;
    use crate::marmot::session::PublishWork;
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

    #[tokio::test]
    async fn group_forensics_reads_darkmatter_group_without_obsolete_mls_storage() {
        let (wn, _data_dir, _logs_dir) = create_mock_whitenoise().await;
        let creator = wn.create_identity().await.unwrap();
        let member = wn.create_identity().await.unwrap();
        let creator_session = wn.require_session(&creator.pubkey).unwrap();
        let member_session = wn.require_session(&member.pubkey).unwrap();

        let member_key_package = {
            let member_marmot = member_session.marmot.as_ref().unwrap().clone();
            let mut member_marmot = member_marmot.lock().await;
            let key_package = member_marmot.fresh_key_package().await.unwrap();
            cgka_traits::engine::KeyPackage::with_source_event_id(
                key_package.bytes().to_vec(),
                MessageId::new(vec![0xA9; 32]),
            )
        };

        let created = {
            let creator_marmot = creator_session.marmot.as_ref().unwrap().clone();
            let mut creator_marmot = creator_marmot.lock().await;
            let created = creator_marmot
                .create_group(CreateGroupRequest {
                    name: "debug forensics".to_string(),
                    description: "Darkmatter debug group".to_string(),
                    members: vec![member_key_package],
                    required_features: Vec::new(),
                    app_components: vec![nostr_routing_component()],
                    initial_admins: Vec::new(),
                })
                .await
                .unwrap();
            let PublishWork::GroupCreated { pending, .. } = &created.effects.publish[0] else {
                panic!("Darkmatter group creation should produce GroupCreated publish work");
            };
            creator_marmot.confirm_published(*pending).await.unwrap();
            created
        };

        let group_id = GroupId::from_slice(created.group_id.as_slice());
        let forensics = wn.group_forensics(&creator, &group_id).await.unwrap();

        assert_eq!(forensics.epoch, 1);
        assert_eq!(forensics.member_count, 2);
        assert!(forensics.group_id.starts_with("hash:"));
        assert_ne!(forensics.group_id, hex::encode(created.group_id.as_slice()));
    }

    fn nostr_routing_component() -> AppComponentData {
        AppComponentData {
            component_id: NOSTR_ROUTING_COMPONENT_ID,
            data: encode_nostr_routing_v1(
                &NostrRoutingV1::new([0xA9; 32], vec!["wss://debug.example".to_string()]).unwrap(),
            )
            .unwrap(),
        }
    }
}

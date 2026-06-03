use std::collections::BTreeSet;
use std::sync::Arc;

use cgka_traits::capabilities::{Capability, FeatureStatus};
use cgka_traits::storage::StorageError;

use crate::marmot::GroupId;
use crate::marmot::capabilities::{SELF_REMOVE_CODEPOINT, SELF_REMOVE_FEATURE};
use crate::marmot::storage::WhitenoiseMarmotStorage;
use crate::{
    RelayType, perf_instrument,
    whitenoise::{
        Whitenoise,
        accounts::Account,
        error::{Result, WhitenoiseError},
        relays::Relay,
        shared::SharedServices,
        users::User,
    },
};

pub mod blossom_error;
pub(crate) mod media;
mod membership;
mod required_proposals;

pub use membership::{GroupWithInfoAndMembership, GroupWithMembership};
pub use required_proposals::RequiredProposal;
pub use required_proposals::{
    GroupCapabilityUpgradeStatus, RequiredProposalUpgradability, RequiredProposalUpgradeStatus,
};
pub(crate) use required_proposals::{
    project_darkmatter_required_proposals, project_darkmatter_self_remove_upgrade_status,
    validate_required_proposal_upgrade_targets,
};

impl SharedServices {
    #[perf_instrument("groups")]
    pub(crate) async fn resolve_member_delivery_relays(
        self: &Arc<Self>,
        member: &User,
        fallback_account: &Account,
        context: &'static str,
    ) -> Result<Vec<Relay>> {
        let inbox_relays = member.relays(RelayType::Inbox, &self.database).await?;
        if !inbox_relays.is_empty() {
            return Ok(inbox_relays);
        }

        let nip65_relays = member.relays(RelayType::Nip65, &self.database).await?;
        if !nip65_relays.is_empty() {
            return Ok(nip65_relays);
        }

        let fallback_relays = fallback_account.nip65_relays(self).await?;
        if fallback_relays.is_empty() {
            tracing::error!(
                target: "whitenoise::groups",
                context = context,
                "User {} has no inbox or NIP-65 relays and account {} has no fallback relays configured",
                member.pubkey,
                fallback_account.pubkey
            );
            return Err(WhitenoiseError::MissingWelcomeRelays {
                member_pubkey: member.pubkey,
                account_pubkey: fallback_account.pubkey,
            });
        } else {
            tracing::warn!(
                target: "whitenoise::groups",
                context = context,
                "User {} has no inbox or NIP-65 relays, using account {} fallback relays",
                member.pubkey,
                fallback_account.pubkey
            );
        }

        Ok(fallback_relays)
    }
}

impl Whitenoise {
    fn has_existing_marmot_group_projection(
        &self,
        account: &Account,
        marmot_group_id: &cgka_traits::types::GroupId,
    ) -> Result<bool> {
        let marmot_storage_path =
            Account::marmot_storage_path(&account.pubkey, &self.config().data_dir);
        let marmot_db_key_id = Account::marmot_db_key_id(&account.pubkey);
        let Some(marmot_storage) = WhitenoiseMarmotStorage::open_existing_for_account(
            marmot_storage_path,
            self.keyring_service_id(),
            &marmot_db_key_id,
        )?
        else {
            return Ok(false);
        };

        Ok(marmot_storage
            .find_group_projection(marmot_group_id)?
            .is_some())
    }

    /// Resolves a single member for group creation: finds or creates the user record,
    /// syncs relay lists for new users, fetches and validates the key package.
    /// Resolves a single member for group creation: finds or creates the user record,
    /// syncs relay lists for new users, fetches and validates the key package.
    ///
    /// Returns the set of MLS proposal types required by the group's
    /// `RequiredCapabilities` extension, projected onto the whitenoise mirror
    /// enum [`RequiredProposal`].
    ///
    /// # Arguments
    /// * `account` - The account that has access to the group
    /// * `group_id` - The MLS group ID to inspect
    ///
    /// # Returns
    /// * `Ok(BTreeSet<RequiredProposal>)` - The set of required proposal
    ///   types. An empty set is the LCD outcome for mixed or empty-invitee
    ///   groups and is **not** an error — it is distinct from
    ///   [`WhitenoiseError::GroupNotFound`], which means the MLS record is
    ///   missing.
    ///
    /// # Errors
    /// * `WhitenoiseError::GroupNotFound` - If no MLS record exists for
    ///   `group_id`
    /// * `WhitenoiseError` - If loading the group record fails
    ///
    /// # Examples
    /// ```ignore
    /// let required = wn.group_required_proposals(&account, &group_id).await?;
    /// if required.contains(&RequiredProposal::SelfRemove) {
    ///     // Non-admin members can leave without an admin commit.
    /// }
    /// ```
    #[perf_instrument("groups")]
    pub async fn group_required_proposals(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<BTreeSet<RequiredProposal>> {
        let marmot_group_id = cgka_traits::types::GroupId::new(group_id.as_slice().to_vec());
        if let Some(session) = self.session(&account.pubkey) {
            if let Some(marmot) = &session.marmot {
                let result = {
                    let marmot = marmot.lock().await;
                    marmot.group_required_capabilities(&marmot_group_id)
                };

                match result {
                    Ok(required) => {
                        return Ok(project_darkmatter_required_proposals(required.proposals));
                    }
                    Err(WhitenoiseError::MarmotStorage(StorageError::NotFound)) => {}
                    Err(error) => return Err(error),
                }
            }

            if session
                .marmot_storage
                .find_group_projection(&marmot_group_id)?
                .is_some()
            {
                return Err(WhitenoiseError::MarmotSessionUnavailable(account.pubkey));
            }

            return Err(WhitenoiseError::GroupNotFound);
        }

        if self.has_existing_marmot_group_projection(account, &marmot_group_id)? {
            return Err(WhitenoiseError::MarmotSessionUnavailable(account.pubkey));
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    /// Returns readiness for RequiredCapabilities proposal upgrades.
    #[perf_instrument("groups")]
    pub async fn group_capability_upgrade_status(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<GroupCapabilityUpgradeStatus> {
        let marmot_group_id = cgka_traits::types::GroupId::new(group_id.as_slice().to_vec());
        if let Some(session) = self.session(&account.pubkey) {
            if let Some(marmot) = &session.marmot {
                let result = {
                    let marmot = marmot.lock().await;
                    marmot
                        .group_required_capabilities(&marmot_group_id)
                        .and_then(|_| {
                            let status =
                                marmot.feature_status(&marmot_group_id, &SELF_REMOVE_FEATURE)?;
                            let blockers = if matches!(&status, FeatureStatus::Unavailable { .. }) {
                                marmot.members_missing_capability(
                                    &marmot_group_id,
                                    Capability::Proposal(SELF_REMOVE_CODEPOINT),
                                )?
                            } else {
                                Vec::new()
                            };

                            Ok(project_darkmatter_self_remove_upgrade_status(
                                status, blockers,
                            ))
                        })
                };

                match result {
                    Ok(status) => return Ok(status),
                    Err(WhitenoiseError::MarmotStorage(StorageError::NotFound)) => {}
                    Err(error) => return Err(error),
                }
            }

            if session
                .marmot_storage
                .find_group_projection(&marmot_group_id)?
                .is_some()
            {
                return Err(WhitenoiseError::MarmotSessionUnavailable(account.pubkey));
            }

            return Err(WhitenoiseError::GroupNotFound);
        }

        if self.has_existing_marmot_group_projection(account, &marmot_group_id)? {
            return Err(WhitenoiseError::MarmotSessionUnavailable(account.pubkey));
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    /// Adds proposal types to a group's `RequiredCapabilities` extension.
    ///
    /// Use [`Self::group_capability_upgrade_status`] first to discover which
    /// proposals are upgradeable today; this method enforces the same
    /// capability gate (every member must already advertise the capability) and
    /// surfaces blockers as [`WhitenoiseError::CapabilityUpgradeBlocked`].
    ///
    /// Per MIP-03, the evolution event is published to the group's relays
    /// *before* merging the pending commit locally — local state only
    /// advances once a relay has accepted the event.
    ///
    /// # Arguments
    /// * `account` - The account performing the upgrade (must be group admin)
    /// * `group_id` - The MLS group ID to upgrade
    /// * `proposals_to_add` - Non-empty set of proposals to require; must not
    ///   contain [`RequiredProposal::Unknown`]
    ///
    /// # Errors
    /// * [`WhitenoiseError::InvalidInput`] - `proposals_to_add` is empty or
    ///   contains [`RequiredProposal::Unknown`]
    /// * [`WhitenoiseError::AccountNotAuthorized`] - `account` is not a group
    ///   admin
    /// * [`WhitenoiseError::GroupNotFound`] - No MLS record exists for
    ///   `group_id`
    /// * [`WhitenoiseError::CapabilityUpgradeBlocked`] - One or more members
    ///   do not yet advertise the requested capability
    ///
    /// Idempotent when every requested proposal is already required.
    #[perf_instrument("groups")]
    pub async fn upgrade_group_required_proposals(
        &self,
        account: &Account,
        group_id: &GroupId,
        proposals_to_add: BTreeSet<RequiredProposal>,
    ) -> Result<()> {
        if proposals_to_add.is_empty() {
            return Err(WhitenoiseError::InvalidInput(
                "proposals_to_add must be non-empty".to_string(),
            ));
        }

        let marmot_group_id = cgka_traits::types::GroupId::new(group_id.as_slice().to_vec());
        let session = self.require_session(&account.pubkey)?;

        if let Some(marmot) = &session.marmot {
            let result = {
                let marmot = marmot.lock().await;
                marmot
                    .group_required_capabilities(&marmot_group_id)
                    .and_then(|_| {
                        let status =
                            marmot.feature_status(&marmot_group_id, &SELF_REMOVE_FEATURE)?;
                        let blockers = if matches!(&status, FeatureStatus::Unavailable { .. }) {
                            marmot.members_missing_capability(
                                &marmot_group_id,
                                Capability::Proposal(SELF_REMOVE_CODEPOINT),
                            )?
                        } else {
                            Vec::new()
                        };

                        Ok(project_darkmatter_self_remove_upgrade_status(
                            status, blockers,
                        ))
                    })
            };

            match result {
                Ok(status) => {
                    let admins = session.groups().admins(group_id)?;
                    if !admins.contains(&account.pubkey) {
                        return Err(WhitenoiseError::AccountNotAuthorized);
                    }
                    validate_required_proposal_upgrade_targets(&proposals_to_add)?;
                    ensure_marmot_required_proposals_upgrade_is_safe(&proposals_to_add, &status)?;
                    tracing::info!(
                        target: "whitenoise::groups::upgrade_group_required_proposals",
                        added = ?proposals_to_add,
                        "marmot group required proposals already satisfy requested upgrade"
                    );
                    return Ok(());
                }
                Err(WhitenoiseError::MarmotStorage(StorageError::NotFound)) => {}
                Err(error) => return Err(error),
            }
        }

        if session
            .marmot_storage
            .find_group_projection(&marmot_group_id)?
            .is_some()
        {
            let admins = session.groups().admins(group_id)?;
            if !admins.contains(&account.pubkey) {
                return Err(WhitenoiseError::AccountNotAuthorized);
            }
            validate_required_proposal_upgrade_targets(&proposals_to_add)?;
            return Err(WhitenoiseError::MarmotSessionUnavailable(account.pubkey));
        }

        Err(WhitenoiseError::GroupNotFound)
    }
}

fn ensure_marmot_required_proposals_upgrade_is_safe(
    proposals_to_add: &BTreeSet<RequiredProposal>,
    status: &GroupCapabilityUpgradeStatus,
) -> Result<()> {
    for proposal in proposals_to_add {
        let state = status
            .per_proposal
            .iter()
            .find(|entry| entry.proposal == *proposal)
            .map(|entry| &entry.state)
            .ok_or_else(|| {
                WhitenoiseError::Internal(format!(
                    "Marmot capability status did not include {proposal:?}"
                ))
            })?;

        match state {
            RequiredProposalUpgradability::AlreadyRequired => {}
            RequiredProposalUpgradability::Blocked { blockers } => {
                return Err(WhitenoiseError::CapabilityUpgradeBlocked {
                    proposal: *proposal,
                    blockers: blockers.clone(),
                });
            }
            RequiredProposalUpgradability::Available => {
                return Err(WhitenoiseError::UnsupportedMarmotOperation(
                    "selective RequiredCapabilities upgrades are not available yet; refusing to \
                     promote every currently-upgradeable Darkmatter capability"
                        .to_string(),
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use cgka_traits::agent_text_stream::AgentTextStreamQuicPolicyV1;
    use cgka_traits::app_components::{
        AGENT_TEXT_STREAM_QUIC_COMPONENT_ID, AppComponentData,
        GROUP_MESSAGE_RETENTION_COMPONENT_ID, NOSTR_ROUTING_COMPONENT_ID, NostrRoutingV1,
        decode_nostr_routing_v1, encode_nostr_routing_v1,
    };
    use cgka_traits::engine::CreateGroupRequest;
    use cgka_traits::transport::TransportMessage;
    use cgka_traits::types::MessageId;
    use nostr_blossom::bud02::BlobDescriptor;
    use nostr_sdk::prelude::Url;
    use nostr_sdk::prelude::hashes::Hash as _;
    use nostr_sdk::prelude::hashes::sha256::Hash as Sha256Hash;
    use nostr_sdk::{Keys, Kind, PublicKey, RelayUrl, Timestamp};

    use super::*;
    use crate::marmot::publish::{MarmotMessagePublisher, MarmotPublishOutcome};
    use crate::marmot::session::PublishWork;
    use crate::marmot::{
        GroupConfig, GroupDataUpdate, MarmotCreatedGroupProjection, Secret, group_types,
    };
    use crate::whitenoise::Whitenoise;
    use crate::whitenoise::accounts_groups::AccountGroup;
    use crate::whitenoise::database::media_files::MediaFile;
    use crate::whitenoise::group_information::{GroupInformation, GroupType};
    use crate::whitenoise::key_packages::MLS_KEY_PACKAGE_KIND;
    use crate::whitenoise::media_processing::MediaProcessingOptions;
    use crate::whitenoise::message_aggregator::DeliveryStatus;
    use crate::whitenoise::test_utils::*;

    fn mock_blossom_url(server: &mockito::Server) -> Url {
        let socket_address = server.socket_address();
        Url::parse(&format!("http://{socket_address}")).unwrap()
    }

    fn mock_blob_descriptor(server_url: &Url, blob: &[u8], mime_type: &str) -> BlobDescriptor {
        let sha256 = Sha256Hash::hash(blob);
        let url = server_url.join(&sha256.to_string()).unwrap();

        BlobDescriptor {
            url,
            sha256,
            size: blob.len().try_into().unwrap(),
            mime_type: Some(mime_type.to_string()),
            uploaded: Timestamp::now(),
        }
    }

    async fn create_darkmatter_group_for_testing(
        whitenoise: &Whitenoise,
    ) -> (Account, group_types::Group) {
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(whitenoise, 1).await;
        let member_pubkeys = vec![members[0].0.pubkey];
        let config = create_group_config(vec![creator_account.pubkey]);
        let group = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(member_pubkeys, config, None)
            .await
            .unwrap();

        (creator_account, group)
    }

    #[tokio::test]
    async fn test_project_created_marmot_group_records() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let member_key_package = {
            let member_marmot = member_session.marmot.as_ref().unwrap().clone();
            let mut member_marmot = member_marmot.lock().await;
            key_package_with_source_event_id(member_marmot.fresh_key_package().await.unwrap(), 0xA1)
        };
        let creator_marmot = creator_session.marmot.as_ref().unwrap().clone();

        let created = {
            let mut creator_marmot = creator_marmot.lock().await;
            let created = creator_marmot
                .create_group(CreateGroupRequest {
                    name: "".to_string(),
                    description: "darkmatter direct message".to_string(),
                    members: vec![member_key_package],
                    required_features: Vec::new(),
                    app_components: vec![nostr_routing_component_for_testing()],
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

        let projection = {
            let creator_marmot = creator_marmot.lock().await;
            creator_marmot
                .created_group_projection(&created.group_id)
                .unwrap()
        };
        let projected_group_id = creator_session
            .groups()
            .project_created_marmot_group(&projection, &[member_account.pubkey], None)
            .await
            .unwrap();

        assert_eq!(projected_group_id.as_slice(), created.group_id.as_slice());

        let group_info = GroupInformation::find_by_mls_group_id(
            &projected_group_id,
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        assert_eq!(group_info.mls_group_id, projected_group_id);
        assert_eq!(group_info.group_type, GroupType::DirectMessage);

        let account_group = AccountGroup::find_by_account_and_group(
            &creator_account.pubkey,
            &projected_group_id,
            &creator_session.account_db.inner.pool,
        )
        .await
        .unwrap()
        .expect("projected account group");
        assert!(account_group.is_accepted());
        assert_eq!(account_group.dm_peer_pubkey, Some(member_account.pubkey));
    }

    #[tokio::test]
    async fn test_projected_marmot_group_is_readable_through_group_ops() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let member_key_package = {
            let member_marmot = member_session.marmot.as_ref().unwrap().clone();
            let mut member_marmot = member_marmot.lock().await;
            key_package_with_source_event_id(member_marmot.fresh_key_package().await.unwrap(), 0xA2)
        };
        let creator_marmot = creator_session.marmot.as_ref().unwrap().clone();

        let created = {
            let mut creator_marmot = creator_marmot.lock().await;
            let created = creator_marmot
                .create_group(CreateGroupRequest {
                    name: "readable marmot group".to_string(),
                    description: "projected into existing group reads".to_string(),
                    members: vec![member_key_package],
                    required_features: Vec::new(),
                    app_components: vec![nostr_routing_component_for_testing()],
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

        let projection = {
            let creator_marmot = creator_marmot.lock().await;
            creator_marmot
                .created_group_projection(&created.group_id)
                .unwrap()
        };
        let projected_group_id = creator_session
            .groups()
            .project_created_marmot_group(&projection, &[member_account.pubkey], None)
            .await
            .unwrap();

        let group = creator_session.groups().get(&projected_group_id).unwrap();
        assert_eq!(group.mls_group_id, projected_group_id);
        assert_eq!(group.name, "readable marmot group");
        assert_eq!(group.description, "projected into existing group reads");
        assert_eq!(group.nostr_group_id, [0xA7; 32]);
        assert_eq!(group.state, group_types::GroupState::Active);
        assert!(group.admin_pubkeys.contains(&creator_account.pubkey));

        let all_groups = creator_session.groups().all(true).unwrap();
        assert!(
            all_groups
                .iter()
                .any(|group| group.mls_group_id == projected_group_id)
        );

        let visible_groups = creator_session.groups().visible().await.unwrap();
        assert!(
            visible_groups
                .iter()
                .any(|group| group.group.mls_group_id == projected_group_id)
        );
    }

    #[tokio::test]
    async fn test_resolve_marmot_member_key_package_uses_darkmatter_v2_event() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let resolved = creator_session
            .groups()
            .resolve_marmot_member_key_package(&member_account.pubkey)
            .await
            .unwrap();

        assert_eq!(resolved.user.pubkey, member_account.pubkey);
        assert_eq!(resolved.event.kind, MLS_KEY_PACKAGE_KIND);
        assert_eq!(resolved.event.pubkey, member_account.pubkey);

        let metadata = cgka_engine::key_package_metadata(&resolved.key_package).unwrap();
        assert_eq!(
            metadata.credential_identity_hex,
            member_account.pubkey.to_hex()
        );

        let source = resolved
            .key_package
            .source
            .as_ref()
            .expect("resolved key package should preserve source event id");
        let event_id_bytes = resolved.event.id.to_bytes();
        assert_eq!(source.event_id.as_slice(), event_id_bytes.as_slice());
    }

    #[tokio::test]
    async fn test_prepare_marmot_group_creation_maps_public_config() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let config = GroupConfig::new(
            "mapped group".to_string(),
            "mapped description".to_string(),
            None,
            None,
            None,
            vec![
                RelayUrl::parse("wss://relay-b.example").unwrap(),
                RelayUrl::parse("wss://relay-a.example").unwrap(),
                RelayUrl::parse("wss://relay-a.example").unwrap(),
            ],
            vec![creator_account.pubkey, member_account.pubkey],
            Some(3_600),
        );

        let plan = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .prepare_marmot_group_creation_with_nostr_group_id(
                vec![member_account.pubkey],
                config,
                [0x42; 32],
            )
            .await
            .unwrap();

        assert_eq!(plan.request.name, "mapped group");
        assert_eq!(plan.request.description, "mapped description");
        assert_eq!(plan.request.members.len(), 1);
        assert_eq!(
            plan.request.initial_admins,
            vec![cgka_traits::types::MemberId::new(
                member_account.pubkey.as_bytes().to_vec()
            )]
        );
        assert_eq!(plan.resolved_members.len(), 1);
        assert_eq!(plan.resolved_members[0].user.pubkey, member_account.pubkey);

        let routing = plan
            .request
            .app_components
            .iter()
            .find(|component| component.component_id == NOSTR_ROUTING_COMPONENT_ID)
            .map(|component| decode_nostr_routing_v1(&component.data).unwrap())
            .expect("Nostr routing component");
        assert_eq!(routing.nostr_group_id, [0x42; 32]);
        assert_eq!(
            routing.relays,
            vec![
                "wss://relay-a.example".to_string(),
                "wss://relay-b.example".to_string(),
            ]
        );
        assert_eq!(plan.routing, routing);

        let retention = plan
            .request
            .app_components
            .iter()
            .find(|component| component.component_id == GROUP_MESSAGE_RETENTION_COMPONENT_ID)
            .expect("message retention component");
        assert_eq!(retention.data, 3_600_u64.to_be_bytes().to_vec());

        let agent_text_stream = plan
            .request
            .app_components
            .iter()
            .find(|component| component.component_id == AGENT_TEXT_STREAM_QUIC_COMPONENT_ID)
            .expect("agent text stream component");
        let policy =
            AgentTextStreamQuicPolicyV1::decode_component_state(&agent_text_stream.data).unwrap();
        assert_eq!(policy, AgentTextStreamQuicPolicyV1::user_to_agent_default());
    }

    fn key_package_with_source_event_id(
        key_package: cgka_traits::engine::KeyPackage,
        marker: u8,
    ) -> cgka_traits::engine::KeyPackage {
        cgka_traits::engine::KeyPackage::with_source_event_id(
            key_package.bytes().to_vec(),
            MessageId::new(vec![marker; 32]),
        )
    }

    fn nostr_routing_component_for_testing() -> AppComponentData {
        AppComponentData {
            component_id: NOSTR_ROUTING_COMPONENT_ID,
            data: encode_nostr_routing_v1(
                &NostrRoutingV1::new([0xA7; 32], vec!["wss://group.example".to_string()]).unwrap(),
            )
            .unwrap(),
        }
    }

    fn unprojected_group_id(marker: u8) -> GroupId {
        GroupId::from_slice(&[marker; 32])
    }

    async fn insert_account_fixture(whitenoise: &Whitenoise) -> Account {
        let keys = Keys::generate();
        let pubkey = keys.public_key();
        insert_test_account(&whitenoise.shared.database, &pubkey).await;
        Account::find_by_pubkey(&pubkey, &whitenoise.shared.database)
            .await
            .unwrap()
    }

    async fn insert_projected_session_without_marmot(
        whitenoise: &Arc<Whitenoise>,
        account: &Account,
        group_marker: u8,
    ) -> GroupId {
        let session_without_marmot = Arc::new(
            crate::whitenoise::session::AccountSession::new(
                account.pubkey,
                whitenoise.shared.clone(),
                Arc::downgrade(whitenoise),
                None,
                None,
            )
            .await
            .unwrap(),
        );
        assert!(session_without_marmot.marmot.is_none());

        let marmot_group_id = cgka_traits::types::GroupId::new(vec![group_marker; 32]);
        let group_id = GroupId::from(&marmot_group_id);
        session_without_marmot
            .marmot_storage
            .put_group_projection(&MarmotCreatedGroupProjection {
                group_id: marmot_group_id,
                name: "projected group".to_string(),
                description: "projected without live Marmot session".to_string(),
                epoch: 1,
                routing: NostrRoutingV1::new(
                    [group_marker; 32],
                    vec!["wss://projected-capabilities.example".to_string()],
                )
                .unwrap(),
                admin_pubkeys: BTreeSet::from([account.pubkey]),
                member_pubkeys: BTreeSet::from([account.pubkey]),
                self_update_completed_at_secs: 1,
                disappearing_message_secs: None,
            })
            .unwrap();

        whitenoise
            .account_manager
            .insert_session(session_without_marmot);

        group_id
    }

    #[tokio::test]
    async fn test_group_required_proposals_missing_group_does_not_create_obsolete_mls_storage() {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[0xA9; 32]);
        let artifacts = remove_obsolete_mls_artifacts(&account, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        let result = whitenoise
            .group_required_proposals(&account, &group_id)
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn group_required_proposals_projected_group_without_marmot_does_not_create_obsolete_mls_storage()
     {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = insert_account_fixture(&whitenoise).await;
        let artifacts = remove_obsolete_mls_artifacts(&account, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        let group_id = insert_projected_session_without_marmot(&whitenoise, &account, 0xAB).await;

        let result = whitenoise
            .group_required_proposals(&account, &group_id)
            .await;

        assert!(matches!(
            result,
            Err(WhitenoiseError::MarmotSessionUnavailable(pubkey)) if pubkey == account.pubkey
        ));
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn group_required_proposals_projected_group_without_session_does_not_create_obsolete_mls_storage()
     {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = insert_account_fixture(&whitenoise).await;
        let artifacts = remove_obsolete_mls_artifacts(&account, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        let group_id = insert_projected_session_without_marmot(&whitenoise, &account, 0xAD).await;
        whitenoise.account_manager.remove_session(&account.pubkey);
        assert!(whitenoise.session(&account.pubkey).is_none());

        let result = whitenoise
            .group_required_proposals(&account, &group_id)
            .await;

        assert!(matches!(
            result,
            Err(WhitenoiseError::MarmotSessionUnavailable(pubkey)) if pubkey == account.pubkey
        ));
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn group_required_proposals_unprojected_group_returns_group_not_found() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = unprojected_group_id(0xAF);

        let result = whitenoise
            .group_required_proposals(&account, &group_id)
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn test_group_capability_upgrade_status_missing_group_does_not_create_obsolete_mls_storage()
     {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[0xAA; 32]);
        let artifacts = remove_obsolete_mls_artifacts(&account, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        let result = whitenoise
            .group_capability_upgrade_status(&account, &group_id)
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn group_capability_upgrade_status_projected_group_without_marmot_does_not_create_obsolete_mls_storage()
     {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = insert_account_fixture(&whitenoise).await;
        let artifacts = remove_obsolete_mls_artifacts(&account, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        let group_id = insert_projected_session_without_marmot(&whitenoise, &account, 0xAC).await;

        let result = whitenoise
            .group_capability_upgrade_status(&account, &group_id)
            .await;

        assert!(matches!(
            result,
            Err(WhitenoiseError::MarmotSessionUnavailable(pubkey)) if pubkey == account.pubkey
        ));
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn group_capability_upgrade_status_projected_group_without_session_does_not_create_obsolete_mls_storage()
     {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = insert_account_fixture(&whitenoise).await;
        let artifacts = remove_obsolete_mls_artifacts(&account, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        let group_id = insert_projected_session_without_marmot(&whitenoise, &account, 0xAE).await;
        whitenoise.account_manager.remove_session(&account.pubkey);
        assert!(whitenoise.session(&account.pubkey).is_none());

        let result = whitenoise
            .group_capability_upgrade_status(&account, &group_id)
            .await;

        assert!(matches!(
            result,
            Err(WhitenoiseError::MarmotSessionUnavailable(pubkey)) if pubkey == account.pubkey
        ));
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[derive(Clone)]
    struct RecordingMarmotPublisher {
        published: Arc<AtomicUsize>,
    }

    impl RecordingMarmotPublisher {
        fn new() -> Self {
            Self {
                published: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn published_count(&self) -> usize {
            self.published.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl MarmotMessagePublisher for RecordingMarmotPublisher {
        async fn publish(&self, _message: TransportMessage) -> MarmotPublishOutcome {
            self.published.fetch_add(1, Ordering::SeqCst);
            MarmotPublishOutcome::Published { accepted_count: 1 }
        }
    }

    #[tokio::test]
    async fn test_create_marmot_group_with_publisher_confirms_and_projects_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let config = GroupConfig::new(
            "created by marmot".to_string(),
            "published and projected through GroupOps".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("wss://group-create.example").unwrap()],
            vec![creator_account.pubkey],
            Some(60),
        );
        let publisher = RecordingMarmotPublisher::new();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();

        let group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member_account.pubkey],
                config,
                None,
                &publisher,
            )
            .await
            .unwrap();

        assert!(
            publisher.published_count() > 0,
            "Darkmatter group creation should publish welcome transport messages"
        );
        assert_eq!(group.name, "created by marmot");
        assert_eq!(
            group.description,
            "published and projected through GroupOps"
        );
        assert_eq!(group.disappearing_message_secs, Some(60));
        assert_eq!(group.state, group_types::GroupState::Active);
        assert!(group.admin_pubkeys.contains(&creator_account.pubkey));

        let members = creator_session
            .groups()
            .members(&group.mls_group_id)
            .unwrap();
        assert_eq!(members.len(), 2);
        assert!(members.contains(&creator_account.pubkey));
        assert!(members.contains(&member_account.pubkey));

        let relays = creator_session
            .groups()
            .relays(&group.mls_group_id)
            .unwrap();
        assert_eq!(
            relays,
            BTreeSet::from([RelayUrl::parse("wss://group-create.example").unwrap()])
        );

        let visible_groups = creator_session.groups().visible().await.unwrap();
        assert!(
            visible_groups
                .iter()
                .any(|visible| visible.group.mls_group_id == group.mls_group_id)
        );
    }

    #[tokio::test]
    async fn test_marmot_group_member_mutations_update_group_reads() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let initial_member = whitenoise.create_identity().await.unwrap();
        let added_member = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&initial_member, &added_member]).await;

        let config = GroupConfig::new(
            "mutable marmot group".to_string(),
            "membership changes stay in Darkmatter projection".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("wss://group-members.example").unwrap()],
            vec![creator_account.pubkey],
            None,
        );
        let publisher = RecordingMarmotPublisher::new();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![initial_member.pubkey],
                config,
                None,
                &publisher,
            )
            .await
            .unwrap();

        creator_session
            .groups()
            .add_marmot_members_with_publisher(
                &group.mls_group_id,
                vec![added_member.pubkey],
                &publisher,
            )
            .await
            .unwrap();

        let members_after_add = creator_session
            .groups()
            .members(&group.mls_group_id)
            .unwrap();
        assert_eq!(members_after_add.len(), 3);
        assert!(members_after_add.contains(&creator_account.pubkey));
        assert!(members_after_add.contains(&initial_member.pubkey));
        assert!(members_after_add.contains(&added_member.pubkey));

        creator_session
            .groups()
            .remove_marmot_members_with_publisher(
                &group.mls_group_id,
                vec![initial_member.pubkey],
                &publisher,
            )
            .await
            .unwrap();

        let members_after_remove = creator_session
            .groups()
            .members(&group.mls_group_id)
            .unwrap();
        assert_eq!(members_after_remove.len(), 2);
        assert!(members_after_remove.contains(&creator_account.pubkey));
        assert!(members_after_remove.contains(&added_member.pubkey));
        assert!(!members_after_remove.contains(&initial_member.pubkey));
    }

    #[tokio::test]
    async fn test_marmot_group_data_update_refreshes_group_reads() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let config = GroupConfig::new(
            "original marmot group".to_string(),
            "original description".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("wss://old-group.example").unwrap()],
            vec![creator_account.pubkey],
            None,
        );
        let publisher = RecordingMarmotPublisher::new();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member_account.pubkey],
                config,
                None,
                &publisher,
            )
            .await
            .unwrap();

        let update = GroupDataUpdate::new()
            .name("updated marmot group")
            .description("updated description")
            .relays(vec![
                RelayUrl::parse("wss://updated-b.example").unwrap(),
                RelayUrl::parse("wss://updated-a.example").unwrap(),
            ])
            .admins(vec![creator_account.pubkey, member_account.pubkey])
            .disappearing_message_secs(Some(600));

        creator_session
            .groups()
            .update_marmot_group_data_with_publisher(&group.mls_group_id, update, &publisher)
            .await
            .unwrap();

        let updated = creator_session.groups().get(&group.mls_group_id).unwrap();
        assert_eq!(updated.name, "updated marmot group");
        assert_eq!(updated.description, "updated description");
        assert_eq!(updated.disappearing_message_secs, Some(600));
        assert!(updated.admin_pubkeys.contains(&creator_account.pubkey));
        assert!(updated.admin_pubkeys.contains(&member_account.pubkey));

        let relays = creator_session
            .groups()
            .relays(&group.mls_group_id)
            .unwrap();
        assert_eq!(
            relays,
            BTreeSet::from([
                RelayUrl::parse("wss://updated-a.example").unwrap(),
                RelayUrl::parse("wss://updated-b.example").unwrap(),
            ])
        );
    }

    #[tokio::test]
    async fn test_marmot_self_demote_refreshes_admin_projection() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let config = GroupConfig::new(
            "self demote marmot group".to_string(),
            "admin policy changes stay in Darkmatter projection".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("wss://self-demote.example").unwrap()],
            vec![creator_account.pubkey, member_account.pubkey],
            None,
        );
        let publisher = RecordingMarmotPublisher::new();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member_account.pubkey],
                config,
                None,
                &publisher,
            )
            .await
            .unwrap();

        creator_session
            .groups()
            .self_demote_marmot_with_publisher(&group.mls_group_id, &publisher)
            .await
            .unwrap();

        let updated = creator_session.groups().get(&group.mls_group_id).unwrap();
        assert!(!updated.admin_pubkeys.contains(&creator_account.pubkey));
        assert!(updated.admin_pubkeys.contains(&member_account.pubkey));
        assert_eq!(updated.admin_pubkeys.len(), 1);
    }

    #[tokio::test]
    async fn test_marmot_leave_marks_account_group_as_self_removed() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let config = GroupConfig::new(
            "leave marmot group".to_string(),
            "SelfRemove proposals update local membership projection".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("wss://leave.example").unwrap()],
            vec![creator_account.pubkey, member_account.pubkey],
            None,
        );
        let publisher = RecordingMarmotPublisher::new();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member_account.pubkey],
                config,
                None,
                &publisher,
            )
            .await
            .unwrap();

        creator_session
            .groups()
            .self_demote_marmot_with_publisher(&group.mls_group_id, &publisher)
            .await
            .unwrap();

        creator_session
            .groups()
            .leave_marmot_group_with_publisher(&group.mls_group_id, &publisher)
            .await
            .unwrap();

        let account_group = AccountGroup::find_by_account_and_group(
            &creator_account.pubkey,
            &group.mls_group_id,
            &creator_session.account_db.inner.pool,
        )
        .await
        .unwrap()
        .expect("creator account group should still be visible after leaving");
        assert!(account_group.is_removed());
        assert!(account_group.self_removed);
    }

    #[tokio::test]
    async fn group_required_proposals_returns_self_remove_for_marmot_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let config = GroupConfig::new(
            "required proposals marmot group".to_string(),
            "required proposals read from Darkmatter state".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("wss://required-proposals.example").unwrap()],
            vec![creator_account.pubkey],
            None,
        );
        let publisher = RecordingMarmotPublisher::new();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member_account.pubkey],
                config,
                None,
                &publisher,
            )
            .await
            .unwrap();

        let required = whitenoise
            .group_required_proposals(&creator_account, &group.mls_group_id)
            .await
            .unwrap();

        assert_eq!(required, BTreeSet::from([RequiredProposal::SelfRemove]));
    }

    #[tokio::test]
    async fn group_capability_upgrade_status_reports_self_remove_for_marmot_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let config = GroupConfig::new(
            "capability status marmot group".to_string(),
            "capability status read from Darkmatter state".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("wss://capability-status.example").unwrap()],
            vec![creator_account.pubkey],
            None,
        );
        let publisher = RecordingMarmotPublisher::new();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member_account.pubkey],
                config,
                None,
                &publisher,
            )
            .await
            .unwrap();

        let status = whitenoise
            .group_capability_upgrade_status(&creator_account, &group.mls_group_id)
            .await
            .unwrap();

        assert_eq!(
            status.per_proposal,
            vec![RequiredProposalUpgradeStatus {
                proposal: RequiredProposal::SelfRemove,
                state: RequiredProposalUpgradability::AlreadyRequired,
            }],
        );
    }

    #[tokio::test]
    async fn upgrade_group_required_proposals_is_idempotent_for_marmot_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let config = GroupConfig::new(
            "capability upgrade marmot group".to_string(),
            "capability upgrade reads from Darkmatter state".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("wss://capability-upgrade.example").unwrap()],
            vec![creator_account.pubkey],
            None,
        );
        let publisher = RecordingMarmotPublisher::new();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member_account.pubkey],
                config,
                None,
                &publisher,
            )
            .await
            .unwrap();

        whitenoise
            .upgrade_group_required_proposals(
                &creator_account,
                &group.mls_group_id,
                BTreeSet::from([RequiredProposal::SelfRemove]),
            )
            .await
            .unwrap();

        let required = whitenoise
            .group_required_proposals(&creator_account, &group.mls_group_id)
            .await
            .unwrap();

        assert_eq!(required, BTreeSet::from([RequiredProposal::SelfRemove]));
    }

    #[tokio::test]
    async fn upgrade_group_required_proposals_projected_group_without_marmot_does_not_create_obsolete_mls_storage()
     {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let config = GroupConfig::new(
            "projected group without runtime".to_string(),
            "capability upgrade must not fall back to new obsolete MLS storage".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("wss://capability-upgrade.example").unwrap()],
            vec![creator_account.pubkey],
            None,
        );
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member_account.pubkey],
                config,
                None,
                &RecordingMarmotPublisher::new(),
            )
            .await
            .unwrap();
        let artifacts = remove_obsolete_mls_artifacts(&creator_account, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        let session_without_marmot = Arc::new(
            crate::whitenoise::session::AccountSession::new(
                creator_account.pubkey,
                whitenoise.shared.clone(),
                Arc::downgrade(&whitenoise),
                None,
                None,
            )
            .await
            .unwrap(),
        );
        assert!(session_without_marmot.marmot.is_none());
        assert!(
            session_without_marmot
                .groups()
                .get(&group.mls_group_id)
                .is_ok()
        );
        whitenoise
            .account_manager
            .insert_session(session_without_marmot);

        let err = whitenoise
            .upgrade_group_required_proposals(
                &creator_account,
                &group.mls_group_id,
                BTreeSet::from([RequiredProposal::SelfRemove]),
            )
            .await
            .expect_err("projected group without Marmot runtime must not create legacy MLS");

        assert!(matches!(
            err,
            WhitenoiseError::MarmotSessionUnavailable(pubkey)
                if pubkey == creator_account.pubkey
        ));
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn test_send_message_to_marmot_group_uses_darkmatter_projection() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let config = GroupConfig::new(
            "message group".to_string(),
            "darkmatter-backed outgoing message".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("wss://group-message.example").unwrap()],
            vec![creator_account.pubkey],
            None,
        );
        let publisher = RecordingMarmotPublisher::new();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member_account.pubkey],
                config,
                None,
                &publisher,
            )
            .await
            .unwrap();

        let sent = creator_session
            .messages()
            .for_group(&group.mls_group_id)
            .send("hello through darkmatter".to_string(), 9, None)
            .await
            .unwrap();

        assert_eq!(sent.message.message.content, "hello through darkmatter");
        assert_eq!(sent.message.message.mls_group_id, group.mls_group_id);
        assert_eq!(sent.message.message.pubkey, creator_account.pubkey);
        assert_eq!(sent.message.message.kind, Kind::from(9));
        assert!(!sent.last_message_deleted);

        let cached = creator_session
            .messages()
            .for_group(&group.mls_group_id)
            .fetch_by_id(&sent.message.message.id.to_hex())
            .await
            .unwrap()
            .expect("sent message should be cached optimistically");
        assert_eq!(cached.content, "hello through darkmatter");
        assert_eq!(cached.delivery_status, Some(DeliveryStatus::Sending));
    }

    #[tokio::test]
    async fn test_create_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Setup creator account
        let creator_account = whitenoise.create_identity().await.unwrap();

        // Setup member accounts
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let mut member_pubkeys = Vec::new();
        for _ in 0..2 {
            let member_account = whitenoise.create_identity().await.unwrap();
            let member_user =
                User::find_by_pubkey(&member_account.pubkey, &whitenoise.shared.database)
                    .await
                    .unwrap();
            creator_session
                .repos
                .follows
                .add(&member_user)
                .await
                .unwrap();
            member_pubkeys.push(member_account.pubkey);
        }

        // Setup admin accounts (creator + one member as admin)
        let admin_pubkeys = vec![creator_account.pubkey, member_pubkeys[0]];

        // Test for success case
        case_create_group_success(
            &whitenoise,
            &creator_account,
            member_pubkeys.clone(),
            admin_pubkeys.clone(),
        )
        .await;

        // Test case: Empty admin list
        case_create_group_empty_admin_list(
            &whitenoise,
            &creator_account,
            member_pubkeys.clone(),
            vec![], // Empty admin list
        )
        .await;

        // Test case: Invalid admin pubkey (not a member)
        let non_member_pubkey = create_test_keys().public_key();
        case_create_group_invalid_admin_pubkey(
            &whitenoise,
            &creator_account,
            member_pubkeys.clone(),
            vec![creator_account.pubkey, non_member_pubkey],
        )
        .await;

        // Test case: DirectMessage group (2 participants total)
        case_create_direct_message_group(
            &whitenoise,
            &creator_account,
            vec![member_pubkeys[0]], // Only one member for DM
            vec![creator_account.pubkey, member_pubkeys[0]],
        )
        .await;
    }

    async fn case_create_group_success(
        whitenoise: &Whitenoise,
        creator_account: &Account,
        member_pubkeys: Vec<PublicKey>,
        admin_pubkeys: Vec<PublicKey>,
    ) {
        let config = create_group_config(admin_pubkeys.clone());
        // Create the group
        let result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(member_pubkeys.clone(), config.clone(), None)
            .await;

        // Assert the group was created successfully
        assert!(result.is_ok(), "Error {:?}", result.unwrap_err());
        let group = result.unwrap();

        // Verify group metadata matches configuration
        assert_eq!(group.name, config.name);
        assert_eq!(group.description, config.description);
        assert_eq!(group.image_hash, config.image_hash);
        assert_eq!(group.image_key, config.image_key.map(Secret::new));

        // Verify admin configuration
        assert_eq!(group.admin_pubkeys.len(), admin_pubkeys.len());
        for admin_pk in &admin_pubkeys {
            assert!(
                group.admin_pubkeys.contains(admin_pk),
                "Admin {} not found in group.admin_pubkeys",
                admin_pk
            );
        }

        // Verify group state and type
        // Just check that group is in a valid state (we can't verify exact state without knowing the enum path)

        // Verify group information was created properly
        let group_info = GroupInformation::get_by_mls_group_id(
            creator_account.pubkey,
            &group.mls_group_id,
            whitenoise,
        )
        .await
        .unwrap();
        assert_eq!(group_info.mls_group_id, group.mls_group_id);
        assert_eq!(group_info.group_type, GroupType::Group);
        // Note: participant_count is stored separately and managed by the GroupInformation logic

        // Verify group members can be retrieved
        let members = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .members(&group.mls_group_id)
            .unwrap();
        assert_eq!(members.len(), member_pubkeys.len() + 1); // +1 for creator
        assert!(
            members.contains(&creator_account.pubkey),
            "Creator not in member list"
        );
        for member_pk in &member_pubkeys {
            assert!(
                members.contains(member_pk),
                "Member {} not found in group",
                member_pk
            );
        }

        // Verify group admins can be retrieved
        let admins = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .admins(&group.mls_group_id)
            .unwrap();
        assert_eq!(admins.len(), admin_pubkeys.len());
        for admin_pk in &admin_pubkeys {
            assert!(
                admins.contains(admin_pk),
                "Admin {} not found in admin list",
                admin_pk
            );
        }

        // Verify AccountGroup was created and auto-accepted for the creator
        let session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let account_group = AccountGroup::find_by_account_and_group(
            &creator_account.pubkey,
            &group.mls_group_id,
            &session.account_db.inner.pool,
        )
        .await
        .unwrap();
        assert!(
            account_group.is_some(),
            "AccountGroup should be created for creator"
        );
        let account_group = account_group.unwrap();
        assert!(
            account_group.is_accepted(),
            "AccountGroup should be auto-accepted for creator"
        );
    }

    /// Test case: Member/admin validation fails - empty admin list
    async fn case_create_group_empty_admin_list(
        whitenoise: &Whitenoise,
        creator_account: &Account,
        member_pubkeys: Vec<PublicKey>,
        admin_pubkeys: Vec<PublicKey>,
    ) {
        let config = create_group_config(admin_pubkeys.clone());
        let result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(member_pubkeys, config.clone(), None)
            .await;

        // Should fail because creator must be included in the admin set.
        assert!(result.is_err());
        match result.unwrap_err() {
            WhitenoiseError::InvalidInput(message) => {
                assert_eq!(message, "creator must be an admin");
            }
            other => panic!(
                "Expected InvalidInput for empty admin list, got: {:?}",
                other
            ),
        }
    }

    /// Test case: Key package fetching fails - invalid member pubkey
    async fn _case_create_group_key_package_fetch_fails(
        whitenoise: &Whitenoise,
        creator_account: &Account,
        member_pubkeys: Vec<PublicKey>,
        admin_pubkeys: Vec<PublicKey>,
    ) {
        let config = create_group_config(admin_pubkeys);
        let result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(member_pubkeys, config, None)
            .await;

        // Should fail because key package doesn't exist for the member
        assert!(result.is_err(), "{:?}", result);
    }

    /// Test case: Member/admin validation fails - non-existent admin
    async fn case_create_group_invalid_admin_pubkey(
        whitenoise: &Whitenoise,
        creator_account: &Account,
        member_pubkeys: Vec<PublicKey>,
        admin_pubkeys: Vec<PublicKey>,
    ) {
        let config = create_group_config(admin_pubkeys);
        let result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(member_pubkeys, config, None)
            .await;

        // Should fail because admin must be a member
        assert!(result.is_err());
        match result.unwrap_err() {
            WhitenoiseError::InvalidInput(message) => {
                assert_eq!(message, "admin must be a member");
            }
            other => panic!(
                "Expected InvalidInput for invalid admin pubkey, got: {:?}",
                other
            ),
        }
    }

    async fn case_create_direct_message_group(
        whitenoise: &Whitenoise,
        creator_account: &Account,
        member_pubkeys: Vec<PublicKey>,
        admin_pubkeys: Vec<PublicKey>,
    ) {
        // Direct message group should have exactly 1 member (plus creator = 2 total)
        assert_eq!(
            member_pubkeys.len(),
            1,
            "Direct message group should have exactly 1 member"
        );
        assert_eq!(
            admin_pubkeys.len(),
            2,
            "Direct message group should have 2 admins (both participants)"
        );

        let mut config = create_group_config(admin_pubkeys.clone());
        config.name = "".to_string();
        let result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(member_pubkeys.clone(), config, None)
            .await;

        assert!(result.is_ok(), "Error {:?}", result.unwrap_err());
        let group = result.unwrap();

        // Verify it's automatically classified as DirectMessage type
        let group_info = GroupInformation::get_by_mls_group_id(
            creator_account.pubkey,
            &group.mls_group_id,
            whitenoise,
        )
        .await
        .unwrap();
        assert_eq!(group_info.mls_group_id, group.mls_group_id);
        assert_eq!(group_info.group_type, GroupType::DirectMessage);
        // DirectMessage groups should have exactly 2 participants (verified via member count below)

        // Verify both participants are admins (standard for DM groups)
        let admins = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .admins(&group.mls_group_id)
            .unwrap();
        assert_eq!(admins.len(), 2, "DirectMessage group should have 2 admins");
        assert!(
            admins.contains(&creator_account.pubkey),
            "Creator should be admin"
        );
        assert!(
            admins.contains(&member_pubkeys[0]),
            "Member should be admin"
        );

        // Verify membership
        let members = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .members(&group.mls_group_id)
            .unwrap();
        assert_eq!(
            members.len(),
            2,
            "DirectMessage group should have exactly 2 members"
        );
        assert!(
            members.contains(&creator_account.pubkey),
            "Creator should be member"
        );
        assert!(
            members.contains(&member_pubkeys[0]),
            "Member should be member"
        );
    }

    #[tokio::test]
    async fn test_group_member_management() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Setup creator and initial members
        let creator_account = whitenoise.create_identity().await.unwrap();
        let initial_members = setup_multiple_test_accounts(&whitenoise, 2).await;
        let initial_member_pubkeys = initial_members
            .iter()
            .map(|(acc, _)| acc.pubkey)
            .collect::<Vec<_>>();

        // Create group with initial members
        let admin_pubkeys = vec![creator_account.pubkey];
        let config = create_group_config(admin_pubkeys.clone());
        let group = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(initial_member_pubkeys.clone(), config, None)
            .await
            .unwrap();

        // Verify initial membership
        let members = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .members(&group.mls_group_id)
            .unwrap();
        assert_eq!(members.len(), 3); // creator + 2 initial members

        // Add new members
        let new_members = setup_multiple_test_accounts(&whitenoise, 2).await;
        let new_member_pubkeys = new_members
            .iter()
            .map(|(acc, _)| acc.pubkey)
            .collect::<Vec<_>>();

        let add_result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .add_members(&group.mls_group_id, new_member_pubkeys.clone())
            .await;
        assert!(
            add_result.is_ok(),
            "Failed to add members: {:?}",
            add_result.unwrap_err()
        );

        // Verify new membership count
        let updated_members = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .members(&group.mls_group_id)
            .unwrap();
        assert_eq!(updated_members.len(), 5); // creator + 2 initial + 2 new
        for new_member_pk in &new_member_pubkeys {
            assert!(
                updated_members.contains(new_member_pk),
                "New member {} not found",
                new_member_pk
            );
        }

        // Remove one member
        let member_to_remove = vec![initial_member_pubkeys[0]];
        let remove_result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .remove_members(&group.mls_group_id, member_to_remove.clone())
            .await;
        assert!(
            remove_result.is_ok(),
            "Failed to remove member: {:?}",
            remove_result.unwrap_err()
        );

        // Verify final membership
        let final_members = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .members(&group.mls_group_id)
            .unwrap();
        assert_eq!(final_members.len(), 4); // creator + 1 remaining initial + 2 new
        assert!(
            !final_members.contains(&member_to_remove[0]),
            "Removed member still in group"
        );
    }

    #[tokio::test]
    async fn test_update_group_data() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Setup creator and member
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkeys = vec![members[0].0.pubkey];

        // Create group
        let admin_pubkeys = vec![creator_account.pubkey];
        let config = create_group_config(admin_pubkeys.clone());
        let group = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(member_pubkeys, config, None)
            .await
            .unwrap();

        // Update group data
        let new_group_data = GroupDataUpdate {
            name: Some("Updated Group Name".to_string()),
            description: Some("Updated description".to_string()),
            image_hash: None,
            image_key: None,
            image_nonce: None,
            image_upload_key: None,
            admins: None,
            relays: None,
            nostr_group_id: None,
            disappearing_message_secs: None,
        };

        let update_result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .update_group_data(&group.mls_group_id, new_group_data.clone())
            .await;
        assert!(
            update_result.is_ok(),
            "Failed to update group data: {:?}",
            update_result.unwrap_err()
        );

        // Verify the group data was updated
        let updated_groups = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .all(true)
            .unwrap();
        let updated_group = updated_groups
            .iter()
            .find(|g| g.mls_group_id == group.mls_group_id)
            .expect("Updated group not found");

        assert_eq!(updated_group.name, new_group_data.name.unwrap());
        assert_eq!(
            updated_group.description,
            new_group_data.description.unwrap()
        );
        assert_eq!(updated_group.image_hash, None);
        assert_eq!(updated_group.image_key, None);

        let unsupported_image_update = GroupDataUpdate {
            name: None,
            description: None,
            image_hash: Some(Some([3u8; 32])),
            image_key: Some(Some([4u8; 32])),
            image_nonce: Some(Some([5u8; 12])),
            image_upload_key: None,
            admins: None,
            relays: None,
            nostr_group_id: None,
            disappearing_message_secs: None,
        };
        let err = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .update_group_data(&group.mls_group_id, unsupported_image_update)
            .await
            .unwrap_err();
        assert!(matches!(err, WhitenoiseError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn test_admin_only_group_functions_reject_non_admin_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 2).await;
        let new_admin_pubkey = members[0].0.pubkey;
        let other_member_pubkey = members[1].0.pubkey;

        let config = create_group_config(vec![creator_account.pubkey]);
        let group = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![new_admin_pubkey, other_member_pubkey], config, None)
            .await
            .unwrap();

        let transfer_admin_rights_update = GroupDataUpdate {
            name: None,
            description: None,
            image_hash: None,
            image_key: None,
            image_nonce: None,
            image_upload_key: None,
            admins: Some(vec![new_admin_pubkey]),
            relays: None,
            nostr_group_id: None,
            disappearing_message_secs: None,
        };
        whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .update_group_data(&group.mls_group_id, transfer_admin_rights_update)
            .await
            .unwrap();

        let new_account = whitenoise.create_identity().await.unwrap();
        let add_members_result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .add_members(&group.mls_group_id, vec![new_account.pubkey])
            .await;
        assert!(
            matches!(
                add_members_result,
                Err(WhitenoiseError::AccountNotAuthorized)
            ),
            "Expected AccountNotAuthorized for add_members_to_group, got: {:?}",
            add_members_result
        );

        let remove_members_result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .remove_members(&group.mls_group_id, vec![other_member_pubkey])
            .await;
        assert!(
            matches!(
                remove_members_result,
                Err(WhitenoiseError::AccountNotAuthorized)
            ),
            "Expected AccountNotAuthorized for remove_members_from_group, got: {:?}",
            remove_members_result
        );

        let update = GroupDataUpdate {
            name: Some("Updated Name".to_string()),
            description: None,
            image_hash: None,
            image_key: None,
            image_nonce: None,
            image_upload_key: None,
            admins: None,
            relays: None,
            nostr_group_id: None,
            disappearing_message_secs: None,
        };
        let update_result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .update_group_data(&group.mls_group_id, update)
            .await;
        assert!(
            matches!(update_result, Err(WhitenoiseError::AccountNotAuthorized)),
            "Expected AccountNotAuthorized for update_group_data, got: {:?}",
            update_result
        );

        let upgrade_result = whitenoise
            .upgrade_group_required_proposals(
                &creator_account,
                &group.mls_group_id,
                BTreeSet::from([RequiredProposal::SelfRemove]),
            )
            .await;
        assert!(
            matches!(upgrade_result, Err(WhitenoiseError::AccountNotAuthorized)),
            "Expected AccountNotAuthorized for upgrade_group_required_proposals, got: {:?}",
            upgrade_result
        );

        let upgrade_unknown_result = whitenoise
            .upgrade_group_required_proposals(
                &creator_account,
                &group.mls_group_id,
                BTreeSet::from([RequiredProposal::Unknown]),
            )
            .await;
        assert!(
            matches!(
                upgrade_unknown_result,
                Err(WhitenoiseError::AccountNotAuthorized)
            ),
            "Expected AccountNotAuthorized to take precedence over invalid input shape, got: {:?}",
            upgrade_unknown_result
        );
    }

    #[cfg(test)]
    async fn set_user_relays(
        whitenoise: &Whitenoise,
        user: &User,
        relay_type: RelayType,
        relay_urls: &[&str],
    ) -> Vec<RelayUrl> {
        let existing_relays = user
            .relays(relay_type, &whitenoise.shared.database)
            .await
            .unwrap();
        for relay in existing_relays {
            user.remove_relay(&relay, relay_type, &whitenoise.shared.database)
                .await
                .unwrap();
        }

        let mut configured_urls = Vec::new();
        for url in relay_urls {
            let relay_url = RelayUrl::parse(url).unwrap();
            let relay = whitenoise
                .find_or_create_relay_by_url(&relay_url)
                .await
                .unwrap();
            user.add_relay(&relay, relay_type, &whitenoise.shared.database)
                .await
                .unwrap();
            configured_urls.push(relay_url);
        }

        configured_urls
    }

    #[tokio::test]
    async fn test_resolve_member_delivery_relays_prefers_inbox_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let fallback_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();

        let fallback_user = fallback_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        set_user_relays(
            &whitenoise,
            &fallback_user,
            RelayType::Nip65,
            &["wss://fallback.example.com"],
        )
        .await;

        let member_user = member_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        set_user_relays(
            &whitenoise,
            &member_user,
            RelayType::Nip65,
            &["wss://member-nip65.example.com"],
        )
        .await;
        let inbox_urls = set_user_relays(
            &whitenoise,
            &member_user,
            RelayType::Inbox,
            &[
                "wss://member-inbox-1.example.com",
                "wss://member-inbox-2.example.com",
            ],
        )
        .await;

        let resolved_relays = whitenoise
            .shared
            .resolve_member_delivery_relays(
                &member_user,
                &fallback_account,
                "tests::resolve_member_delivery_relays_prefers_inbox",
            )
            .await
            .unwrap();

        assert_eq!(Relay::urls(&resolved_relays), inbox_urls);
    }

    #[tokio::test]
    async fn test_resolve_member_delivery_relays_uses_nip65_when_inbox_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let fallback_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();

        let fallback_user = fallback_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        set_user_relays(
            &whitenoise,
            &fallback_user,
            RelayType::Nip65,
            &["wss://fallback.example.com"],
        )
        .await;

        let member_user = member_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        let nip65_urls = set_user_relays(
            &whitenoise,
            &member_user,
            RelayType::Nip65,
            &["wss://member-nip65-only.example.com"],
        )
        .await;
        set_user_relays(&whitenoise, &member_user, RelayType::Inbox, &[]).await;

        let resolved_relays = whitenoise
            .shared
            .resolve_member_delivery_relays(
                &member_user,
                &fallback_account,
                "tests::resolve_member_delivery_relays_uses_nip65_when_inbox_missing",
            )
            .await
            .unwrap();

        assert_eq!(Relay::urls(&resolved_relays), nip65_urls);
    }

    #[tokio::test]
    async fn test_resolve_member_delivery_relays_falls_back_to_account_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let fallback_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();

        let member_user = member_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        set_user_relays(&whitenoise, &member_user, RelayType::Inbox, &[]).await;
        set_user_relays(&whitenoise, &member_user, RelayType::Nip65, &[]).await;

        let fallback_user = fallback_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        let fallback_urls = set_user_relays(
            &whitenoise,
            &fallback_user,
            RelayType::Nip65,
            &["wss://account-fallback.example.com"],
        )
        .await;

        let resolved_relays = whitenoise
            .shared
            .resolve_member_delivery_relays(
                &member_user,
                &fallback_account,
                "tests::resolve_member_delivery_relays_falls_back_to_account",
            )
            .await
            .unwrap();

        assert_eq!(Relay::urls(&resolved_relays), fallback_urls);
    }

    #[tokio::test]
    async fn test_resolve_member_delivery_relays_errors_without_any_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let fallback_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();

        let member_user = member_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        set_user_relays(&whitenoise, &member_user, RelayType::Inbox, &[]).await;
        set_user_relays(&whitenoise, &member_user, RelayType::Nip65, &[]).await;

        let fallback_user = fallback_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        set_user_relays(&whitenoise, &fallback_user, RelayType::Nip65, &[]).await;

        let error = whitenoise
            .shared
            .resolve_member_delivery_relays(
                &member_user,
                &fallback_account,
                "tests::resolve_member_delivery_relays_errors_without_any_relays",
            )
            .await
            .unwrap_err();

        match error {
            WhitenoiseError::MissingWelcomeRelays {
                member_pubkey,
                account_pubkey,
            } => {
                assert_eq!(member_pubkey, member_account.pubkey);
                assert_eq!(account_pubkey, fallback_account.pubkey);
            }
            other => panic!("Expected MissingWelcomeRelays error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_groups_filtering() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Setup accounts
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkeys = vec![members[0].0.pubkey];

        // Create a group
        let admin_pubkeys = vec![creator_account.pubkey];
        let config = create_group_config(admin_pubkeys);
        let _group = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(member_pubkeys, config, None)
            .await
            .unwrap();

        // Test getting all groups
        let all_groups = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .all(false)
            .unwrap();
        assert!(!all_groups.is_empty(), "Should have at least one group");

        // Test getting only active groups
        let active_groups = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .all(true)
            .unwrap();
        assert!(
            !active_groups.is_empty(),
            "Should have at least one active group"
        );

        // All groups should be active in this test case
        assert_eq!(
            all_groups.len(),
            active_groups.len(),
            "All groups should be active"
        );

        // All groups should be in a valid state (exact verification depends on state enum implementation)
    }

    /// Helper to create a group without keeping the auto-accepted AccountGroup.
    /// This allows tests to manually control the AccountGroup state.
    ///
    /// Uses `whitenoise.create_group()` internally but deletes the auto-created
    /// AccountGroup record so tests can create it fresh with the desired state.
    async fn create_group_without_auto_accept(
        whitenoise: &Whitenoise,
        account: &Account,
        member_pubkeys: Vec<PublicKey>,
    ) -> group_types::Group {
        // Create group normally (this auto-accepts)
        let config = create_group_config(vec![account.pubkey]);
        let group = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .groups()
            .create_group(member_pubkeys, config, None)
            .await
            .unwrap();

        // Delete the auto-created AccountGroup so tests can recreate with desired state
        let session = whitenoise.require_session(&account.pubkey).unwrap();
        sqlx::query("DELETE FROM accounts_groups WHERE mls_group_id = ?")
            .bind(group.mls_group_id.as_slice())
            .execute(&session.account_db.inner.pool)
            .await
            .unwrap();

        group
    }

    #[tokio::test]
    async fn test_visible_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkeys = vec![members[0].0.pubkey];

        // Create 3 groups, deleting the auto-accepted AccountGroup
        let group_accepted =
            create_group_without_auto_accept(&whitenoise, &account, member_pubkeys.clone()).await;
        let group_pending =
            create_group_without_auto_accept(&whitenoise, &account, member_pubkeys.clone()).await;
        let group_declined =
            create_group_without_auto_accept(&whitenoise, &account, member_pubkeys).await;

        // Manually create AccountGroup records with different states:
        // - group_accepted: user_confirmation = Some(true)
        // - group_pending: user_confirmation = None (default from get_or_create)
        // - group_declined: user_confirmation = Some(false)

        let (ag_accepted, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_accepted.mls_group_id)
            .get_or_create(None)
            .await
            .unwrap();
        ag_accepted.accept(&whitenoise).await.unwrap();

        // Just create the record - stays pending (NULL) by default
        whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_pending.mls_group_id)
            .get_or_create(None)
            .await
            .unwrap();

        let (ag_declined, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_declined.mls_group_id)
            .get_or_create(None)
            .await
            .unwrap();
        ag_declined.decline(&whitenoise).await.unwrap();

        // Get visible groups - should return accepted + pending, not declined
        let mut visible = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .groups()
            .visible()
            .await
            .unwrap();

        assert_eq!(visible.len(), 2);

        // Sort by membership created_at for deterministic ordering
        visible.sort_by_key(|gwm| gwm.membership.created_at);

        // Verify correct groups and their states
        assert_eq!(visible[0].group.mls_group_id, group_accepted.mls_group_id);
        assert!(visible[0].is_accepted());
        assert!(!visible[0].is_pending());

        assert_eq!(visible[1].group.mls_group_id, group_pending.mls_group_id);
        assert!(visible[1].is_pending());
        assert!(!visible[1].is_accepted());
    }

    #[tokio::test]
    async fn test_visible_groups_with_info_includes_group_information() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 2).await;

        // Regular group (non-empty name → GroupType::Group)
        let regular_group = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .groups()
            .create_group(
                vec![members[0].0.pubkey],
                create_group_config(vec![account.pubkey]),
                None,
            )
            .await
            .unwrap();

        // DM group (empty name → GroupType::DirectMessage)
        let mut dm_config = create_group_config(vec![account.pubkey, members[1].0.pubkey]);
        dm_config.name = "".to_string();
        let dm_group = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![members[1].0.pubkey], dm_config, None)
            .await
            .unwrap();

        let mut with_info = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .groups()
            .visible_with_info()
            .await
            .unwrap();

        // Both groups are visible; GroupInformation is included for each.
        assert_eq!(with_info.len(), 2, "Both groups should be returned");
        with_info.sort_by_key(|g| g.membership.created_at);

        let regular = with_info
            .iter()
            .find(|g| g.group.mls_group_id == regular_group.mls_group_id)
            .expect("regular group not found");
        assert_eq!(regular.info.group_type, GroupType::Group);

        let dm = with_info
            .iter()
            .find(|g| g.group.mls_group_id == dm_group.mls_group_id)
            .expect("DM group not found");
        assert_eq!(dm.info.group_type, GroupType::DirectMessage);
    }

    #[tokio::test]
    async fn test_visible_groups_with_info_repairs_darkmatter_group_information_without_obsolete_mls_storage()
     {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member]).await;
        let artifacts = remove_obsolete_mls_artifacts(&creator, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        let creator_session = whitenoise.require_session(&creator.pubkey).unwrap();
        let config = GroupConfig::new(
            "darkmatter visible with info".to_string(),
            "visible_with_info projection repair".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("ws://localhost:7777").unwrap()],
            vec![creator.pubkey],
            None,
        );
        let group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member.pubkey],
                config,
                None,
                &RecordingMarmotPublisher::new(),
            )
            .await
            .unwrap();

        sqlx::query("DELETE FROM group_information WHERE mls_group_id = ?")
            .bind(group.mls_group_id.as_slice())
            .execute(&whitenoise.shared.database.pool)
            .await
            .unwrap();

        let with_info = creator_session.groups().visible_with_info().await.unwrap();

        assert_eq!(with_info.len(), 1);
        assert_eq!(with_info[0].group.mls_group_id, group.mls_group_id);
        assert_eq!(with_info[0].info.group_type, GroupType::Group);
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn test_visible_groups_with_info_excludes_declined() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 2).await;

        let group_accepted =
            create_group_without_auto_accept(&whitenoise, &account, vec![members[0].0.pubkey])
                .await;
        let group_declined =
            create_group_without_auto_accept(&whitenoise, &account, vec![members[1].0.pubkey])
                .await;

        let (ag_accepted, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_accepted.mls_group_id)
            .get_or_create(None)
            .await
            .unwrap();
        ag_accepted.accept(&whitenoise).await.unwrap();

        let (ag_declined, _) = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_declined.mls_group_id)
            .get_or_create(None)
            .await
            .unwrap();
        ag_declined.decline(&whitenoise).await.unwrap();

        let with_info = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .groups()
            .visible_with_info()
            .await
            .unwrap();

        assert_eq!(with_info.len(), 1, "Declined group should be excluded");
        assert_eq!(with_info[0].group.mls_group_id, group_accepted.mls_group_id);
        assert_eq!(with_info[0].info.group_type, GroupType::Group);
    }

    #[tokio::test]
    async fn test_visible_groups_with_info_caller_can_filter_dms() {
        // Demonstrates the intended usage: caller filters on group_type.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 2).await;

        // One regular group, one DM
        let regular_group = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .groups()
            .create_group(
                vec![members[0].0.pubkey],
                create_group_config(vec![account.pubkey]),
                None,
            )
            .await
            .unwrap();

        let mut dm_config = create_group_config(vec![account.pubkey, members[1].0.pubkey]);
        dm_config.name = "".to_string();
        let _dm = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![members[1].0.pubkey], dm_config, None)
            .await
            .unwrap();

        let non_dms: Vec<_> = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .groups()
            .visible_with_info()
            .await
            .unwrap()
            .into_iter()
            .filter(|g| g.info.group_type == GroupType::Group)
            .collect();

        assert_eq!(non_dms.len(), 1);
        assert_eq!(non_dms[0].group.mls_group_id, regular_group.mls_group_id);
    }

    #[tokio::test]
    async fn test_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Setup creator account
        let creator_account = whitenoise.create_identity().await.unwrap();

        // Setup member accounts
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkeys = vec![members[0].0.pubkey];

        // Create a group
        let admin_pubkeys = vec![creator_account.pubkey];
        let config = create_group_config(admin_pubkeys.clone());
        let created_group = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(member_pubkeys.clone(), config.clone(), None)
            .await
            .unwrap();

        // Test: Successfully retrieve the created group
        let retrieved_group = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .get(&created_group.mls_group_id);

        assert!(
            retrieved_group.is_ok(),
            "Failed to retrieve group: {:?}",
            retrieved_group.unwrap_err()
        );

        let retrieved_group = retrieved_group.unwrap();
        assert_eq!(retrieved_group.mls_group_id, created_group.mls_group_id);
        assert_eq!(retrieved_group.name, config.name);
        assert_eq!(retrieved_group.description, config.description);
        assert_eq!(retrieved_group.admin_pubkeys, created_group.admin_pubkeys);

        // Test: Attempt to retrieve non-existent group
        let fake_group_id = GroupId::from_slice(&[255u8; 32]);
        let result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .get(&fake_group_id);

        assert!(result.is_err(), "Expected error for non-existent group");
        match result.unwrap_err() {
            WhitenoiseError::GroupNotFound => {
                // Expected error type
            }
            other => panic!("Expected GroupNotFound error, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_leave_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Setup creator and members
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 2).await;
        let member_accounts = members.iter().map(|(acc, _)| acc).collect::<Vec<_>>();
        let member_pubkeys = member_accounts
            .iter()
            .map(|acc| acc.pubkey)
            .collect::<Vec<_>>();

        // Create group with creator and members as admins (so they can process the leave proposal)
        let admin_pubkeys = vec![creator_account.pubkey, member_pubkeys[0]];
        let config = create_group_config(admin_pubkeys);
        let group = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(member_pubkeys.clone(), config, None)
            .await
            .unwrap();

        // Verify initial membership
        let initial_members = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .members(&group.mls_group_id)
            .unwrap();
        assert_eq!(initial_members.len(), 3); // creator + 2 members

        // Creator must self-demote before leaving (MIP-03: admins cannot SelfRemove)
        whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .self_demote(&group.mls_group_id)
            .await
            .expect("self_demote should succeed (another admin exists)");

        // Creator leaves the group (creates SelfRemove proposal)
        // Note: In a real scenario, members would need to accept welcome messages
        // to have access to the group. For this test, we use the creator who
        // has immediate access to the group.
        let leave_result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .leave(&group.mls_group_id)
            .await;

        assert!(
            leave_result.is_ok(),
            "Failed to initiate leave group: {:?}",
            leave_result.unwrap_err()
        );

        // Note: At this point, the member has only created a proposal to leave.
        // The actual removal would happen when an admin processes the commit,
        // but that's part of the message processing pipeline that would be
        // tested separately in integration tests.

        // A second leave attempt must be rejected — the account already departed
        let second_leave = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .leave(&group.mls_group_id)
            .await;
        assert!(
            matches!(second_leave, Err(WhitenoiseError::AlreadyDepartedFromGroup)),
            "Expected AlreadyDepartedFromGroup, got: {:?}",
            second_leave
        );
    }

    #[tokio::test]
    async fn test_upload_group_image_rejects_unprojected_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let group_id = unprojected_group_id(0xE1);

        let result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .media()
            .upload_group_image(&group_id, "/does/not/exist.png", None, None)
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn test_get_group_image_path_rejects_unprojected_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let group_id = unprojected_group_id(0xE2);

        let result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .media()
            .get_group_image_path(&group_id)
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn test_sync_group_image_cache_rejects_unprojected_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let group_id = unprojected_group_id(0xE3);

        let result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .media()
            .sync_group_image_cache_if_needed(&group_id)
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn test_upload_chat_media_rejects_unprojected_group_before_file_io() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let group_id = unprojected_group_id(0xE4);

        let result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .media()
            .upload_chat_media(&group_id, "/does/not/exist.png", None, None)
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn test_download_chat_media_rejects_unprojected_group_without_obsolete_mls_decrypt() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let group_id = unprojected_group_id(0xE5);
        let original_file_hash = [0x42; 32];

        let result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .media()
            .download_chat_media(&group_id, &original_file_hash)
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn test_darkmatter_group_media_boundary() {
        use tempfile::NamedTempFile;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let mut blossom_server = mockito::Server::new_async().await;
        let blossom_url = mock_blossom_url(&blossom_server);
        let blossom_url_for_response = blossom_url.clone();
        let _upload_mock = blossom_server
            .mock("PUT", "/upload")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let descriptor = mock_blob_descriptor(
                    &blossom_url_for_response,
                    request.body().unwrap(),
                    "image/png",
                );
                serde_json::to_vec(&descriptor).unwrap()
            })
            .create_async()
            .await;
        let (creator_account, group) = create_darkmatter_group_for_testing(&whitenoise).await;

        let img = ::image::RgbaImage::from_pixel(100, 100, ::image::Rgba([255u8, 0, 0, 255]));
        let temp_file = NamedTempFile::new().unwrap();
        img.save_with_format(temp_file.path(), ::image::ImageFormat::Png)
            .unwrap();
        let temp_path = temp_file.path().to_str().unwrap();

        let group_image_error = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .media()
            .upload_group_image(&group.mls_group_id, temp_path, None, None)
            .await
            .unwrap_err();
        assert!(matches!(
            group_image_error,
            WhitenoiseError::UnsupportedMarmotOperation(_)
        ));

        let chat_media = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .media()
            .upload_chat_media(
                &group.mls_group_id,
                temp_path,
                Some(blossom_url),
                Some(MediaProcessingOptions {
                    generate_blurhash: false,
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert_eq!(chat_media.media_type, "chat_media");
        assert_eq!(chat_media.scheme_version.as_deref(), Some("mip04-v2"));
        assert!(chat_media.original_file_hash.is_some());
        assert!(chat_media.nonce.is_some());
        let cached_plaintext = tokio::fs::read(&chat_media.file_path).await.unwrap();
        let cached_hash = Sha256Hash::hash(&cached_plaintext);
        assert_eq!(
            chat_media.original_file_hash.as_deref(),
            Some(cached_hash.as_ref())
        );

        let image_path = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .media()
            .get_group_image_path(&group.mls_group_id)
            .await
            .unwrap();
        assert!(image_path.is_none());
    }

    #[tokio::test]
    async fn test_sync_group_image_cache_noops_for_darkmatter_group_until_component_exists() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let config = create_group_config(vec![creator_account.pubkey]);
        let group = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![members[0].0.pubkey], config, None)
            .await
            .unwrap();

        whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .media()
            .sync_group_image_cache_if_needed(&group.mls_group_id)
            .await
            .unwrap();

        let cached_path = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .media()
            .get_group_image_path(&group.mls_group_id)
            .await
            .unwrap();
        assert!(cached_path.is_none());
    }

    #[tokio::test]
    async fn test_upload_chat_media() {
        use tempfile::NamedTempFile;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let mut blossom_server = mockito::Server::new_async().await;
        let blossom_url = mock_blossom_url(&blossom_server);
        let blossom_url_for_response = blossom_url.clone();
        let _upload_mock = blossom_server
            .mock("PUT", "/upload")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let descriptor = mock_blob_descriptor(
                    &blossom_url_for_response,
                    request.body().unwrap(),
                    "image/png",
                );
                serde_json::to_vec(&descriptor).unwrap()
            })
            .create_async()
            .await;

        let (creator_account, group) = create_darkmatter_group_for_testing(&whitenoise).await;

        // Create a valid 100x100 PNG image
        let img = ::image::RgbaImage::from_pixel(100, 100, ::image::Rgba([0u8, 255, 0, 255]));
        let temp_file = NamedTempFile::new().unwrap();
        img.save_with_format(temp_file.path(), ::image::ImageFormat::Png)
            .unwrap();
        let temp_path = temp_file.path().to_str().unwrap();

        // Test upload with creator account
        // Note: In a real scenario, members would upload after processing their welcome message,
        // which gives them access to the group secrets needed for encryption key derivation.
        // For this unit test, we use the creator who has immediate group access.
        let test_options = MediaProcessingOptions {
            generate_blurhash: false,
            ..Default::default()
        };
        let result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .media()
            .upload_chat_media(
                &group.mls_group_id,
                temp_path,
                Some(blossom_url),
                Some(test_options),
            )
            .await;

        assert!(
            result.is_ok(),
            "Failed to upload chat image as creator: {:?}",
            result.unwrap_err()
        );

        let media_file = result.unwrap();

        // Verify the media file contains valid data
        assert_ne!(
            media_file.encrypted_file_hash,
            vec![0u8; 32],
            "Encrypted hash should not be all zeros"
        );
        assert!(media_file.blossom_url.is_some(), "URL should be present");
        assert!(
            media_file.nostr_key.is_some(),
            "Nostr key should be stored for chat images"
        );
        assert_eq!(media_file.mime_type, "image/png");
        assert_eq!(media_file.media_type, "chat_media");
        assert!(
            media_file.file_path.exists(),
            "Cached file should exist at: {}",
            media_file.file_path.display()
        );

        // Verify the original filename was stored in metadata
        assert!(media_file.file_metadata.is_some());
        let metadata = media_file.file_metadata.as_ref().unwrap();
        assert!(
            metadata.original_filename.is_some(),
            "Original filename should be stored"
        );
    }

    /// MP4 ftyp header only (matches `types::tests::test_detect_non_image_mp4_video`).
    fn minimal_mp4_fixture() -> Vec<u8> {
        vec![
            0x00, 0x00, 0x00, 0x18, // Box size
            b'f', b't', b'y', b'p', // "ftyp"
            b'i', b's', b'o', b'm', // Brand: isom
            0x00, 0x00, 0x00, 0x00, // Version
            b'i', b's', b'o', b'm', // Compatible brands
            b'm', b'p', b'4', b'2',
        ]
    }

    #[tokio::test]
    async fn test_upload_chat_media_video_stores_original_filename_in_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let mut blossom_server = mockito::Server::new_async().await;
        let blossom_url = mock_blossom_url(&blossom_server);
        let blossom_url_for_response = blossom_url.clone();
        let _upload_mock = blossom_server
            .mock("PUT", "/upload")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let descriptor = mock_blob_descriptor(
                    &blossom_url_for_response,
                    request.body().unwrap(),
                    "video/mp4",
                );
                serde_json::to_vec(&descriptor).unwrap()
            })
            .create_async()
            .await;

        let (creator_account, group) = create_darkmatter_group_for_testing(&whitenoise).await;

        let temp_dir = tempfile::tempdir().unwrap();
        let expected_basename = "video_picker_ABC123.mp4";
        let video_path = temp_dir.path().join(expected_basename);
        tokio::fs::write(&video_path, minimal_mp4_fixture())
            .await
            .unwrap();
        let temp_path = video_path.to_str().unwrap();

        let test_options = MediaProcessingOptions {
            generate_blurhash: false,
            ..Default::default()
        };
        let result = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .media()
            .upload_chat_media(
                &group.mls_group_id,
                temp_path,
                Some(blossom_url),
                Some(test_options),
            )
            .await;

        assert!(
            result.is_ok(),
            "Failed to upload chat video: {:?}",
            result.unwrap_err()
        );

        let media_file = result.unwrap();
        assert_eq!(media_file.mime_type, "video/mp4");
        assert_eq!(media_file.media_type, "chat_media");

        let metadata = media_file
            .file_metadata
            .as_ref()
            .expect("video uploads must persist FileMetadata for MIP-04 decryption");
        assert_eq!(
            metadata.original_filename.as_deref(),
            Some(expected_basename),
            "receiver imeta / decrypt must use the same basename the sender encrypted with"
        );
        assert!(
            metadata.dimensions.is_none(),
            "minimal MP4 fixture has no parsed dimensions"
        );

        let original_hash_bytes: [u8; 32] = media_file
            .original_file_hash
            .as_ref()
            .expect("chat_media should store original_file_hash")
            .as_slice()
            .try_into()
            .expect("original_file_hash must be 32 bytes");

        let session = whitenoise
            .account_manager
            .get_session(&creator_account.pubkey)
            .unwrap();
        let reloaded = MediaFile::find_by_original_hash_and_group(
            &session.account_db.inner.pool,
            &whitenoise.shared.database,
            &creator_account.pubkey,
            &original_hash_bytes,
            &group.mls_group_id,
        )
        .await
        .unwrap()
        .expect("MediaFile row should exist after upload");

        assert_eq!(
            reloaded
                .file_metadata
                .as_ref()
                .and_then(|m| m.original_filename.as_deref()),
            Some(expected_basename),
            "filename metadata must round-trip through SQLite for download/decrypt"
        );
    }

    // ── Ordering tests: publish failure must not advance local state ─────
    //
    // These tests call the actual production methods (add_members_to_group,
    // remove_members_from_group, update_group_data) against groups whose
    // relays are configured to unreachable ports. This ensures the tests
    // exercise the real publish-then-merge ordering and would catch
    // regressions if someone reorders the code in the future.

    /// Unreachable relay URLs used to force publish failures in ordering tests.
    const UNREACHABLE_RELAYS: &[&str] = &["ws://localhost:1", "ws://localhost:2"];

    /// Creates a group whose MLS-stored relays point to unreachable ports.
    ///
    /// The group is created with real Docker relays (so welcome fan-out
    /// succeeds during `create_group`), then a successful `update_group_data`
    /// swaps the relays to unreachable URLs. After this, any subsequent
    /// legacy relay lookup returns the unreachable relays.
    async fn create_group_with_unreachable_relays(
        whitenoise: &Whitenoise,
    ) -> (group_types::Group, Account, Vec<(Account, Keys)>) {
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(whitenoise, 2).await;
        let member_pks = members.iter().map(|(a, _)| a.pubkey).collect::<Vec<_>>();

        // Create with real relays so welcome messages succeed
        let config = create_group_config(vec![creator.pubkey]);
        let group = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .create_group(member_pks, config, None)
            .await
            .unwrap();

        // Now swap the group's relays to unreachable ports via update_group_data
        let unreachable_urls: Vec<RelayUrl> = UNREACHABLE_RELAYS
            .iter()
            .map(|u| RelayUrl::parse(u).unwrap())
            .collect();
        let relay_swap = GroupDataUpdate {
            name: None,
            description: None,
            image_hash: None,
            image_key: None,
            image_nonce: None,
            image_upload_key: None,
            admins: None,
            relays: Some(unreachable_urls),
            nostr_group_id: None,
            disappearing_message_secs: None,
        };
        whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .update_group_data(&group.mls_group_id, relay_swap)
            .await
            .unwrap();

        (group, creator, members)
    }

    #[tokio::test]
    async fn test_add_members_no_merge_on_publish_failure() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (group, creator, _existing) = create_group_with_unreachable_relays(&whitenoise).await;
        let group_id = &group.mls_group_id;

        let members_before = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .members(group_id)
            .unwrap();

        // Prepare a new member with a key package on the real relay
        let new_members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let new_pk = new_members[0].0.pubkey;

        // Pause time so backoff sleeps complete instantly; resume before DB reads.
        tokio::time::pause();
        // Call the actual production method — it will fail at publish
        // because the group's relays are now unreachable.
        let result = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .add_members(group_id, vec![new_pk])
            .await;
        tokio::time::resume();
        assert!(result.is_err(), "Should fail when relays are unreachable");

        // Verify: group membership is unchanged (merge did not happen)
        let members_after = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .members(group_id)
            .unwrap();
        assert_eq!(
            members_before.len(),
            members_after.len(),
            "Member count should be unchanged when publish fails \
             (pending commit must not be merged)"
        );
        assert!(
            !members_after.contains(&new_pk),
            "New member should not appear in the group"
        );
    }

    #[tokio::test]
    async fn test_remove_members_no_merge_on_publish_failure() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (group, creator, existing) = create_group_with_unreachable_relays(&whitenoise).await;
        let group_id = &group.mls_group_id;
        let member_to_remove = existing[0].0.pubkey;

        let members_before = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .members(group_id)
            .unwrap();
        assert!(members_before.contains(&member_to_remove));

        // Pause time so backoff sleeps complete instantly; resume before DB reads.
        tokio::time::pause();
        // Call the actual production method — fails at publish
        let result = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .remove_members(group_id, vec![member_to_remove])
            .await;
        tokio::time::resume();
        assert!(result.is_err(), "Should fail when relays are unreachable");

        // Verify: member is still in the group (merge did not happen)
        let members_after = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .members(group_id)
            .unwrap();
        assert_eq!(
            members_before.len(),
            members_after.len(),
            "Member count should be unchanged when publish fails"
        );
        assert!(
            members_after.contains(&member_to_remove),
            "Removed member should still be present (merge must not have happened)"
        );
    }

    #[tokio::test]
    async fn test_update_group_data_no_merge_on_publish_failure() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (group, creator, _existing) = create_group_with_unreachable_relays(&whitenoise).await;
        let group_id = &group.mls_group_id;

        let group_before = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .get(group_id)
            .unwrap();

        // Pause time so backoff sleeps complete instantly; resume before DB reads.
        tokio::time::pause();
        // Call the actual production method — fails at publish
        let new_data = GroupDataUpdate {
            name: Some("Should Not Appear".to_string()),
            description: Some("This update should not be applied".to_string()),
            image_hash: None,
            image_key: None,
            image_nonce: None,
            image_upload_key: None,
            admins: None,
            relays: None,
            nostr_group_id: None,
            disappearing_message_secs: None,
        };
        let result = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .update_group_data(group_id, new_data)
            .await;
        tokio::time::resume();
        assert!(result.is_err(), "Should fail when relays are unreachable");

        // Verify: group data is unchanged (merge did not happen)
        let group_after = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .get(group_id)
            .unwrap();
        assert_eq!(
            group_before.name, group_after.name,
            "Group name should be unchanged when publish fails"
        );
        assert_eq!(
            group_before.description, group_after.description,
            "Group description should be unchanged when publish fails"
        );
    }

    #[tokio::test]
    async fn group_required_proposals_returns_self_remove_for_native_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();

        let config = create_group_config(vec![creator_account.pubkey]);
        let group = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member_account.pubkey], config, None)
            .await
            .unwrap();

        let required = whitenoise
            .group_required_proposals(&creator_account, &group.mls_group_id)
            .await
            .unwrap();

        assert_eq!(required, BTreeSet::from([RequiredProposal::SelfRemove]));
    }

    #[tokio::test]
    async fn group_required_proposals_errors_on_missing_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let missing_group_id = GroupId::from_slice(&[0; 32]);

        let err = whitenoise
            .group_required_proposals(&account, &missing_group_id)
            .await
            .expect_err("fabricated group ID must error");

        assert!(matches!(err, WhitenoiseError::GroupNotFound));
    }

    #[tokio::test]
    async fn group_capability_upgrade_status_unprojected_group_returns_group_not_found() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let group_id = unprojected_group_id(0xF0);

        let result = whitenoise
            .group_capability_upgrade_status(&creator_account, &group_id)
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn upgrade_group_required_proposals_unprojected_group_returns_group_not_found() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let group_id = unprojected_group_id(0xF1);

        let result = whitenoise
            .upgrade_group_required_proposals(
                &creator_account,
                &group_id,
                BTreeSet::from([RequiredProposal::SelfRemove]),
            )
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn group_capability_upgrade_status_errors_on_missing_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, _keys) = create_test_account(&whitenoise).await;
        let missing_group_id = GroupId::from_slice(&[0; 32]);

        let err = whitenoise
            .group_capability_upgrade_status(&account, &missing_group_id)
            .await
            .expect_err("fabricated group ID must error");

        assert!(matches!(err, WhitenoiseError::GroupNotFound));
    }

    #[tokio::test]
    async fn upgrade_group_required_proposals_rejects_unknown_target() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;
        let config = create_group_config(vec![creator_account.pubkey]);
        let group = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member_account.pubkey], config, None)
            .await
            .unwrap();

        let err = whitenoise
            .upgrade_group_required_proposals(
                &creator_account,
                &group.mls_group_id,
                BTreeSet::from([RequiredProposal::Unknown]),
            )
            .await
            .expect_err("unknown upgrade target must be rejected as invalid input");

        assert!(matches!(err, WhitenoiseError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn upgrade_group_required_proposals_rejects_empty_set_before_group_lookup() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, _keys) = create_test_account(&whitenoise).await;
        let missing_group_id = GroupId::from_slice(&[0; 32]);

        let err = whitenoise
            .upgrade_group_required_proposals(&account, &missing_group_id, BTreeSet::new())
            .await
            .expect_err("empty upgrade target must fail before group lookup");

        assert!(matches!(err, WhitenoiseError::InvalidInput(_)));
    }
}

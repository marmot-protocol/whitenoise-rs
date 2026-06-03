use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use cgka_engine::account_identity_proof::{
    AccountIdentityProofRequest, AccountIdentityProofSigner,
};
use cgka_engine::canonicalization::CanonicalizationPolicy;
use cgka_engine::{Engine, EngineBuilder};
use cgka_traits::TransportGroupSubscription;
use cgka_traits::app_components::{
    AppComponentData, GROUP_MESSAGE_RETENTION_COMPONENT_ID, NOSTR_ROUTING_COMPONENT_ID,
    NostrRoutingV1, decode_nostr_routing_v1,
};
use cgka_traits::capabilities::{Capability, Feature, FeatureStatus, GroupCapabilities};
use cgka_traits::engine::{
    CgkaEngine, CreateGroupRequest, GroupEvent, KeyPackage, SendIntent, SendResult,
};
use cgka_traits::engine_state::PendingStateRef;
use cgka_traits::ingest::IngestOutcome;
use cgka_traits::storage::{CapabilityStorage, GroupStorage, StorageError};
use cgka_traits::transport::{TransportEnvelope, TransportMessage};
use cgka_traits::types::{GroupId, MemberId};
use marmot_forensics::ForensicsExportOptions;
use nostr_sdk::secp256k1::Message;
use nostr_sdk::{Keys, PublicKey, Timestamp};
use transport_nostr_peeler::NostrMlsPeeler;

use super::MarmotCreatedGroupProjection;
use super::forensics::GroupForensics;
use super::media::ENCRYPTED_MEDIA_EXPORTER_LABEL;
use super::storage::WhitenoiseMarmotStorage;
use super::transport::MarmotGroupPublishRoute;
use crate::marmot::app_components::whitenoise_supported_app_components;
use crate::marmot::capabilities::whitenoise_feature_registry;
use crate::whitenoise::error::{Result, WhitenoiseError};

pub(crate) struct MarmotSession {
    engine: Engine<WhitenoiseMarmotStorage>,
    storage: WhitenoiseMarmotStorage,
    account_pubkey: PublicKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MarmotSessionEffects {
    pub(crate) events: Vec<GroupEvent>,
    pub(crate) publish: Vec<PublishWork>,
    pub(crate) queued: Vec<QueuedIntentRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MarmotIngestEffects {
    pub(crate) outcome: IngestOutcome,
    pub(crate) effects: MarmotSessionEffects,
}

impl MarmotSessionEffects {
    fn extend(&mut self, other: Self) {
        self.events.extend(other.events);
        self.publish.extend(other.publish);
        self.queued.extend(other.queued);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PublishWork {
    ApplicationMessage {
        msg: TransportMessage,
    },
    Proposal {
        msg: TransportMessage,
    },
    GroupEvolution {
        msg: TransportMessage,
        welcomes: Vec<TransportMessage>,
        pending: PendingStateRef,
    },
    GroupCreated {
        welcomes: Vec<TransportMessage>,
        pending: PendingStateRef,
    },
    AutoPublish {
        msg: TransportMessage,
        pending: PendingStateRef,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueuedIntentRef {
    pub(crate) group_id: cgka_traits::types::GroupId,
    pub(crate) intent_id: cgka_traits::types::MessageId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreateGroupEffects {
    pub(crate) group_id: cgka_traits::types::GroupId,
    pub(crate) effects: MarmotSessionEffects,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JoinWelcomeEffects {
    pub(crate) group_id: cgka_traits::types::GroupId,
    pub(crate) effects: MarmotSessionEffects,
}

impl MarmotSession {
    pub(crate) fn open_local(
        account_pubkey: PublicKey,
        storage: WhitenoiseMarmotStorage,
        keys: Keys,
    ) -> Result<Self> {
        if keys.public_key() != account_pubkey {
            return Err(WhitenoiseError::InvalidInput(
                "Marmot account proof signer does not match account pubkey".to_string(),
            ));
        }

        let peeler = NostrMlsPeeler::new().with_welcome_signer(keys.clone());
        let mut engine = EngineBuilder::new(storage.clone())
            .identity(account_pubkey.as_bytes().to_vec())
            .account_identity_proof_signer(Arc::new(NostrAccountIdentityProofSigner {
                keys: keys.clone(),
            }))
            .feature_registry(whitenoise_feature_registry())
            .supported_app_components(whitenoise_supported_app_components())
            .peeler(Box::new(peeler))
            .build()?;

        engine.set_convergence_policy(CanonicalizationPolicy {
            settlement_quiescence_ms: 0,
            ..CanonicalizationPolicy::default()
        });
        engine.hydrate_stable_groups_from_storage()?;

        Ok(Self {
            engine,
            storage,
            account_pubkey,
        })
    }

    pub(crate) fn account_pubkey(&self) -> PublicKey {
        self.account_pubkey
    }

    pub(crate) fn self_id(&self) -> MemberId {
        self.engine.self_id()
    }

    pub(crate) fn group_publish_route(
        &self,
        group_id: &GroupId,
    ) -> Result<MarmotGroupPublishRoute> {
        let routing = self.group_routing(group_id)?;

        Ok(MarmotGroupPublishRoute::from_nostr_routing(
            group_id.clone(),
            routing,
        ))
    }

    pub(crate) fn group_publish_route_for_transport_group_id(
        &self,
        transport_group_id: &[u8],
    ) -> Result<MarmotGroupPublishRoute> {
        let direct_group_id = GroupId::new(transport_group_id.to_vec());
        match self.storage.get_group(&direct_group_id) {
            Ok(_) => return self.group_publish_route(&direct_group_id),
            Err(StorageError::NotFound) => {}
            Err(error) => return Err(error.into()),
        }

        for group_id in self.storage.list_groups()? {
            let route = self.group_publish_route(&group_id)?;
            if route.transport_group_id.as_slice() == transport_group_id {
                return Ok(route);
            }
        }

        Err(WhitenoiseError::InvalidInput(format!(
            "missing Marmot group publish route for transport group {}",
            hex::encode(transport_group_id)
        )))
    }

    pub(crate) fn group_subscriptions(&self) -> Result<Vec<TransportGroupSubscription>> {
        self.storage
            .list_groups()?
            .into_iter()
            .map(|group_id| {
                self.group_publish_route(&group_id)
                    .map(MarmotGroupPublishRoute::into_group_subscription)
            })
            .collect()
    }

    pub(crate) fn confirmed_group_count(&self) -> Result<usize> {
        Ok(self.storage.list_groups()?.len())
    }

    pub(crate) fn created_group_projection(
        &self,
        group_id: &GroupId,
    ) -> Result<MarmotCreatedGroupProjection> {
        self.group_projection(group_id)
    }

    pub(crate) fn group_projection(
        &self,
        group_id: &GroupId,
    ) -> Result<MarmotCreatedGroupProjection> {
        let group = self.engine.group_record(group_id)?;
        let admin_pubkeys = self.group_admin_pubkeys(group_id)?;
        let member_pubkeys = group
            .members
            .iter()
            .map(|member| self.member_pubkey(group_id, member.id.as_slice()))
            .collect::<Result<BTreeSet<_>>>()?;

        Ok(MarmotCreatedGroupProjection {
            group_id: group.id,
            name: group.name,
            description: group.description,
            epoch: group.epoch.0,
            routing: self.group_routing(group_id)?,
            admin_pubkeys,
            member_pubkeys,
            self_update_completed_at_secs: Timestamp::now().as_secs(),
            disappearing_message_secs: self.group_disappearing_message_secs(group_id)?,
        })
    }

    pub(crate) fn group_epoch(&self, group_id: &GroupId) -> Result<u64> {
        Ok(self.engine.group_record(group_id)?.epoch.0)
    }

    pub(crate) fn encrypted_media_exporter_secret(
        &self,
        group_id: &GroupId,
    ) -> Result<cgka_traits::SecretBytes> {
        let context = self.engine.group_context(group_id)?;
        context
            .exporter_secret(ENCRYPTED_MEDIA_EXPORTER_LABEL, 32)
            .ok_or_else(|| {
                WhitenoiseError::MarmotEngine(cgka_traits::error::EngineError::Other(
                    "missing encrypted-media exporter secret".to_string(),
                ))
            })
    }

    pub(crate) fn group_required_capabilities(
        &self,
        group_id: &GroupId,
    ) -> Result<GroupCapabilities> {
        Ok(self.storage.get_group(group_id)?.required_capabilities)
    }

    pub(crate) fn group_forensics(&self, group_id: &GroupId) -> Result<GroupForensics> {
        let options = ForensicsExportOptions::public(self.forensics_redaction_salt(group_id));
        Ok(self.engine.group_forensics(group_id, &options)?.into())
    }

    pub(crate) fn feature_status(
        &self,
        group_id: &GroupId,
        feature: &Feature,
    ) -> Result<FeatureStatus> {
        Ok(self.engine.feature_status(group_id, feature)?)
    }

    pub(crate) fn members_missing_capability(
        &self,
        group_id: &GroupId,
        capability: Capability,
    ) -> Result<Vec<PublicKey>> {
        let mut blockers = Vec::new();
        for member in &self.storage.get_group(group_id)?.members {
            let capabilities = self
                .storage
                .member_capabilities(group_id, &member.id)?
                .unwrap_or_default();
            if !capabilities.contains(&capability) {
                blockers.push(self.member_pubkey(group_id, member.id.as_slice())?);
            }
        }

        blockers.sort();
        blockers.dedup();
        Ok(blockers)
    }

    /// Derive the MIP-05 push-token member index from Darkmatter's stable
    /// member ordering. Darkmatter does not expose MLS leaf indices in
    /// [`cgka_traits::group::Member`]; this mirrors `marmot-app`'s current
    /// runtime projection for push-token cache compatibility.
    pub(crate) fn push_member_index_map(
        &self,
        group_id: &GroupId,
    ) -> Result<BTreeMap<u32, PublicKey>> {
        let mut member_index = BTreeMap::new();

        for (index, member) in self.engine.members(group_id)?.into_iter().enumerate() {
            let push_index = u32::try_from(index).map_err(|error| {
                WhitenoiseError::Internal(format!(
                    "too many Marmot members in group {} for MIP-05 push index: {error}",
                    hex::encode(group_id.as_slice())
                ))
            })?;
            member_index.insert(
                push_index,
                self.member_pubkey(group_id, member.id.as_slice())?,
            );
        }

        Ok(member_index)
    }

    pub(crate) async fn fresh_key_package(&mut self) -> Result<KeyPackage> {
        Ok(self.engine.fresh_key_package().await?)
    }

    pub(crate) async fn create_group(
        &mut self,
        req: CreateGroupRequest,
    ) -> Result<CreateGroupEffects> {
        let (group_id, result) = self.engine.create_group(req).await?;
        let effects = self.collect_effects(vec![result]);
        Ok(CreateGroupEffects { group_id, effects })
    }

    pub(crate) async fn join_welcome(
        &mut self,
        welcome_msg: TransportMessage,
    ) -> Result<JoinWelcomeEffects> {
        let group_id = self.engine.join_welcome(welcome_msg).await?;
        let effects = self.collect_effects(vec![]);
        Ok(JoinWelcomeEffects { group_id, effects })
    }

    pub(crate) fn recognizes_transport_message(&self, msg: &TransportMessage) -> Result<bool> {
        match &msg.envelope {
            TransportEnvelope::Welcome { recipient } => Ok(recipient == &self.self_id()),
            TransportEnvelope::GroupMessage { transport_group_id } => {
                self.recognizes_transport_group_id(transport_group_id)
            }
        }
    }

    pub(crate) async fn ingest(&mut self, msg: TransportMessage) -> Result<MarmotIngestEffects> {
        let outcome = self.engine.ingest(msg).await?;
        let mut effects = self.collect_effects(vec![]);
        if let IngestOutcome::Buffered { group_id, .. } = &outcome {
            effects.extend(self.advance_convergence(group_id.clone()).await?);
        }
        Ok(MarmotIngestEffects { outcome, effects })
    }

    pub(crate) async fn advance_convergence(
        &mut self,
        group_id: GroupId,
    ) -> Result<MarmotSessionEffects> {
        let results = CgkaEngine::advance_convergence(&mut self.engine, &group_id).await?;
        Ok(self.collect_effects(results))
    }

    pub(crate) async fn send_app_message(
        &mut self,
        group_id: GroupId,
        payload: Vec<u8>,
    ) -> Result<MarmotSessionEffects> {
        let result = self
            .engine
            .send(SendIntent::AppMessage { group_id, payload })
            .await?;
        Ok(self.collect_effects(vec![result]))
    }

    pub(crate) async fn invite_members(
        &mut self,
        group_id: GroupId,
        key_packages: Vec<KeyPackage>,
    ) -> Result<MarmotSessionEffects> {
        let result = self
            .engine
            .send(SendIntent::Invite {
                group_id,
                key_packages,
            })
            .await?;
        Ok(self.collect_effects(vec![result]))
    }

    pub(crate) async fn remove_members(
        &mut self,
        group_id: GroupId,
        members: Vec<MemberId>,
    ) -> Result<MarmotSessionEffects> {
        let result = self
            .engine
            .send(SendIntent::RemoveMembers { group_id, members })
            .await?;
        Ok(self.collect_effects(vec![result]))
    }

    pub(crate) async fn leave_group(&mut self, group_id: GroupId) -> Result<MarmotSessionEffects> {
        let result = self.engine.send(SendIntent::Leave { group_id }).await?;
        Ok(self.collect_effects(vec![result]))
    }

    pub(crate) async fn update_app_components(
        &mut self,
        group_id: GroupId,
        updates: Vec<AppComponentData>,
    ) -> Result<MarmotSessionEffects> {
        let result = self
            .engine
            .send(SendIntent::UpdateAppComponents { group_id, updates })
            .await?;
        Ok(self.collect_effects(vec![result]))
    }

    pub(crate) async fn confirm_published(
        &mut self,
        pending: PendingStateRef,
    ) -> Result<MarmotSessionEffects> {
        let event = self.engine.confirm_published(pending).await?;
        let mut effects = self.collect_effects(vec![]);
        if !effects.events.contains(&event) {
            effects.events.insert(0, event);
        }
        Ok(effects)
    }

    pub(crate) async fn publish_failed(
        &mut self,
        pending: PendingStateRef,
    ) -> Result<MarmotSessionEffects> {
        self.engine.publish_failed(pending).await?;
        Ok(self.collect_effects(vec![]))
    }

    pub(crate) fn delete_key_package_material(&self, key_package: &KeyPackage) -> Result<()> {
        Ok(self.storage.delete_key_package_material(key_package)?)
    }

    #[cfg(test)]
    pub(crate) fn has_key_package_material(&self, key_package: &KeyPackage) -> Result<bool> {
        Ok(self.storage.has_key_package_material(key_package)?)
    }

    fn collect_effects(&mut self, results: Vec<SendResult>) -> MarmotSessionEffects {
        let mut effects = MarmotSessionEffects {
            events: self.engine.drain_events(),
            publish: Vec::new(),
            queued: Vec::new(),
        };

        for result in results {
            match result {
                SendResult::ApplicationMessage { msg } => effects
                    .publish
                    .push(PublishWork::ApplicationMessage { msg }),
                SendResult::Proposal { msg } => effects.publish.push(PublishWork::Proposal { msg }),
                SendResult::GroupEvolution {
                    msg,
                    welcomes,
                    pending,
                } => effects.publish.push(PublishWork::GroupEvolution {
                    msg,
                    welcomes,
                    pending,
                }),
                SendResult::GroupCreated { welcomes, pending } => effects
                    .publish
                    .push(PublishWork::GroupCreated { welcomes, pending }),
                SendResult::Queued {
                    group_id,
                    intent_id,
                } => effects.queued.push(QueuedIntentRef {
                    group_id,
                    intent_id,
                }),
            }
        }

        for auto in self.engine.drain_auto_publish() {
            effects.publish.push(PublishWork::AutoPublish {
                msg: auto.msg,
                pending: auto.pending,
            });
        }
        effects.events.extend(self.engine.drain_events());

        effects
    }

    fn group_routing(&self, group_id: &GroupId) -> Result<NostrRoutingV1> {
        let Some(routing_bytes) = self
            .engine
            .app_component(group_id, NOSTR_ROUTING_COMPONENT_ID)?
        else {
            return Err(WhitenoiseError::GroupMissingRelays);
        };

        decode_nostr_routing_v1(&routing_bytes).map_err(|error| {
            WhitenoiseError::InvalidInput(format!(
                "invalid marmot.transport.nostr.routing.v1 for group {}: {error}",
                hex::encode(group_id.as_slice())
            ))
        })
    }

    fn forensics_redaction_salt(&self, group_id: &GroupId) -> Vec<u8> {
        let mut salt = b"whitenoise-group-forensics-v1".to_vec();
        salt.extend_from_slice(self.account_pubkey.as_bytes());
        salt.extend_from_slice(group_id.as_slice());
        salt
    }

    fn recognizes_transport_group_id(&self, transport_group_id: &[u8]) -> Result<bool> {
        let direct_group_id = GroupId::new(transport_group_id.to_vec());
        match self.storage.get_group(&direct_group_id) {
            Ok(_) => return Ok(true),
            Err(StorageError::NotFound) => {}
            Err(error) => return Err(error.into()),
        }

        for group_id in self.storage.list_groups()? {
            if group_id.as_slice() == transport_group_id {
                return Ok(true);
            }
            if self.group_routing(&group_id)?.nostr_group_id.as_slice() == transport_group_id {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn group_admin_pubkeys(&self, group_id: &GroupId) -> Result<BTreeSet<PublicKey>> {
        self.engine
            .admin_pubkeys(group_id)?
            .into_iter()
            .map(|bytes| self.member_pubkey(group_id, &bytes))
            .collect()
    }

    fn member_pubkey(&self, group_id: &GroupId, bytes: &[u8]) -> Result<PublicKey> {
        PublicKey::from_slice(bytes).map_err(|error| {
            WhitenoiseError::InvalidInput(format!(
                "invalid Marmot member public key for group {}: {error}",
                hex::encode(group_id.as_slice())
            ))
        })
    }

    fn group_disappearing_message_secs(&self, group_id: &GroupId) -> Result<Option<u64>> {
        match self
            .engine
            .app_component(group_id, GROUP_MESSAGE_RETENTION_COMPONENT_ID)?
        {
            None => Ok(None),
            Some(bytes) => {
                if bytes.len() != 8 {
                    return Err(WhitenoiseError::InvalidInput(format!(
                        "invalid marmot.group.message-retention.v1 for group {}: expected 8 bytes, got {}",
                        hex::encode(group_id.as_slice()),
                        bytes.len()
                    )));
                }

                let mut encoded = [0_u8; 8];
                encoded.copy_from_slice(&bytes);
                match u64::from_be_bytes(encoded) {
                    0 => Ok(None),
                    seconds => Ok(Some(seconds)),
                }
            }
        }
    }
}

#[derive(Clone)]
struct NostrAccountIdentityProofSigner {
    keys: Keys,
}

impl AccountIdentityProofSigner for NostrAccountIdentityProofSigner {
    fn sign_account_identity_proof(
        &self,
        request: &AccountIdentityProofRequest,
    ) -> core::result::Result<[u8; 64], String> {
        if self.keys.public_key().to_bytes().as_slice() != request.account_identity.as_slice() {
            return Err("account identity proof requested for a different account".to_string());
        }

        let message = Message::from_digest_slice(&request.signing_digest())
            .map_err(|err| format!("invalid account identity proof digest: {err}"))?;
        Ok(self.keys.sign_schnorr(&message).serialize())
    }
}

#[cfg(test)]
mod tests {
    use cgka_traits::app_components::{
        AppComponentData, NOSTR_ROUTING_COMPONENT_ID, NostrRoutingV1, encode_nostr_routing_v1,
    };
    use cgka_traits::engine::{CreateGroupRequest, GroupEvent};

    use cgka_traits::types::{MemberId, MessageId};
    use nostr_sdk::Keys;
    use sha2::{Digest, Sha256};

    use super::{MarmotSession, PublishWork};
    use crate::marmot::app_components::whitenoise_supported_app_components;
    use crate::marmot::storage::WhitenoiseMarmotStorage;

    #[tokio::test]
    async fn local_session_uses_nostr_pubkey_as_member_identity() {
        let keys = Keys::generate();
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        let session = MarmotSession::open_local(keys.public_key(), storage, keys.clone()).unwrap();

        assert_eq!(session.account_pubkey(), keys.public_key());
        assert_eq!(
            session.self_id(),
            MemberId::new(keys.public_key().to_bytes().to_vec())
        );
    }

    #[tokio::test]
    async fn local_session_can_create_key_package() {
        let keys = Keys::generate();
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        let mut session =
            MarmotSession::open_local(keys.public_key(), storage, keys.clone()).unwrap();

        let key_package = session.fresh_key_package().await.unwrap();

        assert!(!key_package.bytes().is_empty());
    }

    #[tokio::test]
    async fn local_session_can_delete_fresh_key_package_material() {
        let keys = Keys::generate();
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        let mut session =
            MarmotSession::open_local(keys.public_key(), storage.clone(), keys).unwrap();

        let key_package = session.fresh_key_package().await.unwrap();
        assert!(storage.has_key_package_material(&key_package).unwrap());

        session.delete_key_package_material(&key_package).unwrap();

        assert!(!storage.has_key_package_material(&key_package).unwrap());
    }

    #[test]
    fn local_session_supported_components_match_published_key_package_metadata() {
        let components = whitenoise_supported_app_components()
            .into_iter()
            .collect::<Vec<_>>();

        assert!(components.contains(&0x8001));
        assert!(components.contains(&0x8003));
        assert!(components.contains(&0x8004));
        assert!(components.contains(&0x8005));
        assert!(components.contains(&0x8006));
    }

    #[tokio::test]
    async fn create_group_returns_publish_work_and_confirms_group_created_event() {
        let alice_keys = Keys::generate();
        let bob_keys = Keys::generate();
        let mut alice = MarmotSession::open_local(
            alice_keys.public_key(),
            WhitenoiseMarmotStorage::in_memory().unwrap(),
            alice_keys,
        )
        .unwrap();
        let mut bob = MarmotSession::open_local(
            bob_keys.public_key(),
            WhitenoiseMarmotStorage::in_memory().unwrap(),
            bob_keys,
        )
        .unwrap();
        let bob_key_package =
            key_package_with_event_id(bob.fresh_key_package().await.unwrap(), 0xB0);

        let created = alice
            .create_group(CreateGroupRequest {
                name: "test group".to_string(),
                description: "group created by WhiteNoise MarmotSession".to_string(),
                members: vec![bob_key_package],
                required_features: Vec::new(),
                app_components: vec![nostr_routing_component(nostr_routing(
                    b"session-create-group",
                ))],
                initial_admins: Vec::new(),
            })
            .await
            .unwrap();

        assert!(created.effects.events.is_empty());
        let PublishWork::GroupCreated { welcomes, pending } = &created.effects.publish[0] else {
            panic!("create_group must produce GroupCreated publish work");
        };
        assert_eq!(welcomes.len(), 1);

        let confirmed = alice.confirm_published(*pending).await.unwrap();

        assert!(
            confirmed
                .events
                .iter()
                .any(|event| matches!(event, GroupEvent::GroupCreated { group_id } if group_id == &created.group_id))
        );
    }

    #[tokio::test]
    async fn encrypted_media_exporter_secret_uses_darkmatter_registered_label() {
        let alice_keys = Keys::generate();
        let bob_keys = Keys::generate();
        let mut alice = MarmotSession::open_local(
            alice_keys.public_key(),
            WhitenoiseMarmotStorage::in_memory().unwrap(),
            alice_keys,
        )
        .unwrap();
        let mut bob = MarmotSession::open_local(
            bob_keys.public_key(),
            WhitenoiseMarmotStorage::in_memory().unwrap(),
            bob_keys,
        )
        .unwrap();
        let bob_key_package =
            key_package_with_event_id(bob.fresh_key_package().await.unwrap(), 0xB1);

        let created = alice
            .create_group(CreateGroupRequest {
                name: "media group".to_string(),
                description: String::new(),
                members: vec![bob_key_package],
                required_features: Vec::new(),
                app_components: vec![nostr_routing_component(nostr_routing(
                    b"session-media-exporter",
                ))],
                initial_admins: Vec::new(),
            })
            .await
            .unwrap();
        let PublishWork::GroupCreated { pending, .. } = created.effects.publish[0].clone() else {
            panic!("create_group must produce GroupCreated publish work");
        };
        alice.confirm_published(pending).await.unwrap();

        let secret = alice
            .encrypted_media_exporter_secret(&created.group_id)
            .unwrap();

        assert_eq!(secret.len(), 32);
    }

    #[tokio::test]
    async fn group_publish_route_comes_from_signed_nostr_routing_component() {
        let alice_keys = Keys::generate();
        let bob_keys = Keys::generate();
        let mut alice = MarmotSession::open_local(
            alice_keys.public_key(),
            WhitenoiseMarmotStorage::in_memory().unwrap(),
            alice_keys,
        )
        .unwrap();
        let mut bob = MarmotSession::open_local(
            bob_keys.public_key(),
            WhitenoiseMarmotStorage::in_memory().unwrap(),
            bob_keys,
        )
        .unwrap();
        let bob_key_package =
            key_package_with_event_id(bob.fresh_key_package().await.unwrap(), 0xB2);
        let routing = nostr_routing_with_relays(
            b"session-publish-route",
            vec![
                "wss://relay-b.example".to_string(),
                "wss://relay-a.example".to_string(),
            ],
        );

        let created = alice
            .create_group(CreateGroupRequest {
                name: "routed group".to_string(),
                description: String::new(),
                members: vec![bob_key_package],
                required_features: Vec::new(),
                app_components: vec![nostr_routing_component(routing.clone())],
                initial_admins: Vec::new(),
            })
            .await
            .unwrap();
        let PublishWork::GroupCreated { pending, .. } = &created.effects.publish[0] else {
            panic!("create_group must produce GroupCreated publish work");
        };
        alice.confirm_published(*pending).await.unwrap();

        let route = alice.group_publish_route(&created.group_id).unwrap();

        assert_eq!(route.group_id, created.group_id);
        assert_eq!(route.transport_group_id, routing.nostr_group_id.to_vec());
        assert_eq!(
            route.endpoints,
            vec![
                cgka_traits::TransportEndpoint("wss://relay-a.example".to_string()),
                cgka_traits::TransportEndpoint("wss://relay-b.example".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn group_subscriptions_are_derived_from_confirmed_groups() {
        let alice_keys = Keys::generate();
        let bob_keys = Keys::generate();
        let mut alice = MarmotSession::open_local(
            alice_keys.public_key(),
            WhitenoiseMarmotStorage::in_memory().unwrap(),
            alice_keys,
        )
        .unwrap();
        let mut bob = MarmotSession::open_local(
            bob_keys.public_key(),
            WhitenoiseMarmotStorage::in_memory().unwrap(),
            bob_keys,
        )
        .unwrap();
        let bob_key_package =
            key_package_with_event_id(bob.fresh_key_package().await.unwrap(), 0xB3);
        let routing = nostr_routing_with_relays(
            b"session-group-subscriptions",
            vec![
                "wss://relay-two.example".to_string(),
                "wss://relay-one.example".to_string(),
            ],
        );
        let created = alice
            .create_group(CreateGroupRequest {
                name: "subscription group".to_string(),
                description: String::new(),
                members: vec![bob_key_package],
                required_features: Vec::new(),
                app_components: vec![nostr_routing_component(routing.clone())],
                initial_admins: Vec::new(),
            })
            .await
            .unwrap();
        let PublishWork::GroupCreated { pending, .. } = &created.effects.publish[0] else {
            panic!("create_group must produce GroupCreated publish work");
        };
        alice.confirm_published(*pending).await.unwrap();

        let subscriptions = alice.group_subscriptions().unwrap();

        assert_eq!(subscriptions.len(), 1);
        assert_eq!(subscriptions[0].group_id, created.group_id);
        assert_eq!(
            subscriptions[0].transport_group_id,
            routing.nostr_group_id.to_vec()
        );
        assert_eq!(
            subscriptions[0].endpoints,
            vec![
                cgka_traits::TransportEndpoint("wss://relay-one.example".to_string()),
                cgka_traits::TransportEndpoint("wss://relay-two.example".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn publish_failed_rolls_back_pending_group_create_without_emitting_group_created() {
        let alice_keys = Keys::generate();
        let bob_keys = Keys::generate();
        let mut alice = MarmotSession::open_local(
            alice_keys.public_key(),
            WhitenoiseMarmotStorage::in_memory().unwrap(),
            alice_keys,
        )
        .unwrap();
        let mut bob = MarmotSession::open_local(
            bob_keys.public_key(),
            WhitenoiseMarmotStorage::in_memory().unwrap(),
            bob_keys,
        )
        .unwrap();
        let bob_key_package =
            key_package_with_event_id(bob.fresh_key_package().await.unwrap(), 0xB1);

        let created = alice
            .create_group(CreateGroupRequest {
                name: "rollback group".to_string(),
                description: String::new(),
                members: vec![bob_key_package],
                required_features: Vec::new(),
                app_components: vec![nostr_routing_component(nostr_routing(
                    b"session-rollback-group",
                ))],
                initial_admins: Vec::new(),
            })
            .await
            .unwrap();
        let PublishWork::GroupCreated { pending, .. } = &created.effects.publish[0] else {
            panic!("create_group must produce GroupCreated publish work");
        };

        let rolled_back = alice.publish_failed(*pending).await.unwrap();

        assert!(rolled_back.events.is_empty());
        assert!(
            rolled_back
                .publish
                .iter()
                .all(|work| !matches!(work, PublishWork::GroupCreated { .. }))
        );
    }

    fn key_package_with_event_id(
        key_package: cgka_traits::engine::KeyPackage,
        marker: u8,
    ) -> cgka_traits::engine::KeyPackage {
        cgka_traits::engine::KeyPackage::with_source_event_id(
            key_package.bytes().to_vec(),
            MessageId::new(vec![marker; 32]),
        )
    }

    fn nostr_routing(seed: &[u8]) -> NostrRoutingV1 {
        nostr_routing_with_relays(seed, vec!["wss://group.example".to_string()])
    }

    fn nostr_routing_with_relays(seed: &[u8], relays: Vec<String>) -> NostrRoutingV1 {
        NostrRoutingV1::new(deterministic_nostr_group_id(seed), relays).unwrap()
    }

    fn nostr_routing_component(routing: NostrRoutingV1) -> AppComponentData {
        AppComponentData {
            component_id: NOSTR_ROUTING_COMPONENT_ID,
            data: encode_nostr_routing_v1(&routing).unwrap(),
        }
    }

    fn deterministic_nostr_group_id(seed: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"whitenoise-marmot-session-test-nostr-group-id-v1");
        hasher.update(seed);
        hasher.finalize().into()
    }
}

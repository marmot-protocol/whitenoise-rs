use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use cgka_traits::engine::GroupEvent;
use cgka_traits::engine_state::PendingStateRef;
use cgka_traits::transport::TransportMessage;
use cgka_traits::types::MessageId;
use tokio::sync::Mutex;

use super::session::{MarmotSession, MarmotSessionEffects, PublishWork, QueuedIntentRef};
use crate::whitenoise::error::Result;

#[async_trait]
pub(crate) trait MarmotMessagePublisher {
    async fn publish(&self, message: TransportMessage) -> MarmotPublishOutcome;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MarmotPublishOutcome {
    Published { accepted_count: usize },
    Failed { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MarmotPublishedEffects {
    pub(crate) events: Vec<GroupEvent>,
    pub(crate) queued: Vec<QueuedIntentRef>,
    pub(crate) reports: Vec<MarmotPublishReport>,
    pub(crate) failures: Vec<MarmotPublishFailure>,
    pub(crate) pending: Vec<MarmotPendingResolution>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MarmotPublishReport {
    pub(crate) message_id: MessageId,
    pub(crate) outcome: MarmotPublishOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MarmotPublishFailure {
    pub(crate) message_id: MessageId,
    pub(crate) reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MarmotPendingResolution {
    Confirmed { pending: PendingStateRef },
    RolledBack { pending: PendingStateRef },
}

pub(crate) async fn publish_effects<P>(
    session: Arc<Mutex<MarmotSession>>,
    publisher: &P,
    effects: MarmotSessionEffects,
) -> Result<MarmotPublishedEffects>
where
    P: MarmotMessagePublisher + Sync + ?Sized,
{
    let mut output = MarmotPublishedEffects::new();
    let mut queue = VecDeque::new();
    output.absorb_session_effects(effects, &mut queue);

    while let Some(work) = queue.pop_front() {
        match work {
            PublishWork::ApplicationMessage { msg } | PublishWork::Proposal { msg } => {
                publish_one(publisher, msg, &mut output).await;
            }
            PublishWork::GroupCreated { welcomes, pending } => {
                publish_pending(
                    session.clone(),
                    publisher,
                    welcomes,
                    pending,
                    &mut output,
                    &mut queue,
                )
                .await?;
            }
            PublishWork::GroupEvolution {
                msg,
                welcomes,
                pending,
            } => {
                publish_group_evolution(
                    session.clone(),
                    publisher,
                    msg,
                    welcomes,
                    pending,
                    &mut output,
                    &mut queue,
                )
                .await?;
            }
            PublishWork::AutoPublish { msg, pending } => {
                publish_pending(
                    session.clone(),
                    publisher,
                    vec![msg],
                    pending,
                    &mut output,
                    &mut queue,
                )
                .await?;
            }
        }
    }

    Ok(output)
}

impl MarmotPublishedEffects {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            queued: Vec::new(),
            reports: Vec::new(),
            failures: Vec::new(),
            pending: Vec::new(),
        }
    }

    fn absorb_session_effects(
        &mut self,
        effects: MarmotSessionEffects,
        queue: &mut VecDeque<PublishWork>,
    ) {
        self.events.extend(effects.events);
        self.queued.extend(effects.queued);
        queue.extend(effects.publish);
    }
}

async fn publish_pending<P>(
    session: Arc<Mutex<MarmotSession>>,
    publisher: &P,
    messages: Vec<TransportMessage>,
    pending: PendingStateRef,
    output: &mut MarmotPublishedEffects,
    queue: &mut VecDeque<PublishWork>,
) -> Result<()>
where
    P: MarmotMessagePublisher + Sync + ?Sized,
{
    let mut all_published = true;
    for message in messages {
        all_published &= publish_one(publisher, message, output).await;
    }

    let effects = if all_published {
        let mut session = session.lock().await;
        let effects = session.confirm_published(pending).await?;
        output
            .pending
            .push(MarmotPendingResolution::Confirmed { pending });
        effects
    } else {
        let mut session = session.lock().await;
        let effects = session.publish_failed(pending).await?;
        output
            .pending
            .push(MarmotPendingResolution::RolledBack { pending });
        effects
    };
    output.absorb_session_effects(effects, queue);

    Ok(())
}

async fn publish_group_evolution<P>(
    session: Arc<Mutex<MarmotSession>>,
    publisher: &P,
    commit: TransportMessage,
    welcomes: Vec<TransportMessage>,
    pending: PendingStateRef,
    output: &mut MarmotPublishedEffects,
    queue: &mut VecDeque<PublishWork>,
) -> Result<()>
where
    P: MarmotMessagePublisher + Sync + ?Sized,
{
    if publish_one(publisher, commit, output).await {
        let effects = {
            let mut session = session.lock().await;
            session.confirm_published(pending).await?
        };
        output
            .pending
            .push(MarmotPendingResolution::Confirmed { pending });
        output.absorb_session_effects(effects, queue);

        for welcome in welcomes {
            publish_one(publisher, welcome, output).await;
        }
    } else {
        let effects = {
            let mut session = session.lock().await;
            session.publish_failed(pending).await?
        };
        output
            .pending
            .push(MarmotPendingResolution::RolledBack { pending });
        output.absorb_session_effects(effects, queue);
    }

    Ok(())
}

async fn publish_one<P>(
    publisher: &P,
    message: TransportMessage,
    output: &mut MarmotPublishedEffects,
) -> bool
where
    P: MarmotMessagePublisher + Sync + ?Sized,
{
    let message_id = message.id.clone();
    let outcome = publisher.publish(message).await;
    let published = matches!(outcome, MarmotPublishOutcome::Published { .. });
    if let MarmotPublishOutcome::Failed { reason } = &outcome {
        output.failures.push(MarmotPublishFailure {
            message_id: message_id.clone(),
            reason: reason.clone(),
        });
    }
    output.reports.push(MarmotPublishReport {
        message_id,
        outcome,
    });
    published
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use cgka_traits::app_components::{
        AppComponentData, NOSTR_ROUTING_COMPONENT_ID, NostrRoutingV1, encode_nostr_routing_v1,
    };
    use cgka_traits::engine::{CreateGroupRequest, GroupEvent, KeyPackage};
    use cgka_traits::transport::TransportMessage;
    use cgka_traits::types::MessageId;
    use nostr_sdk::Keys;
    use sha2::{Digest, Sha256};
    use tokio::sync::Mutex;

    use super::{
        MarmotMessagePublisher, MarmotPendingResolution, MarmotPublishOutcome, publish_effects,
    };
    use crate::marmot::session::{MarmotSession, PublishWork};
    use crate::marmot::storage::WhitenoiseMarmotStorage;

    #[tokio::test]
    async fn group_created_work_confirms_after_all_welcomes_publish_without_holding_session_lock() {
        let (alice, mut bob) = local_sessions();
        let bob_key_package =
            key_package_with_event_id(bob.fresh_key_package().await.unwrap(), 0xB0);
        let created = {
            let mut alice = alice.lock().await;
            alice
                .create_group(CreateGroupRequest {
                    name: "coordinator success".to_string(),
                    description: String::new(),
                    members: vec![bob_key_package],
                    required_features: Vec::new(),
                    app_components: vec![nostr_routing_component(b"coordinator-success")],
                    initial_admins: Vec::new(),
                })
                .await
                .unwrap()
        };
        let pending = pending_from_group_created(&created.effects.publish);
        let publisher = RecordingPublisher::new(
            alice.clone(),
            [MarmotPublishOutcome::Published { accepted_count: 1 }],
        );

        let output = publish_effects(alice.clone(), &publisher, created.effects)
            .await
            .unwrap();

        assert!(publisher.lock_was_available_during_publish());
        assert_eq!(publisher.published_count().await, 1);
        assert_eq!(
            output.pending,
            vec![MarmotPendingResolution::Confirmed { pending }]
        );
        assert!(
            output
                .events
                .iter()
                .any(|event| matches!(event, GroupEvent::GroupCreated { group_id } if group_id == &created.group_id))
        );
    }

    #[tokio::test]
    async fn group_created_work_rolls_back_when_any_welcome_publish_fails() {
        let (alice, mut bob) = local_sessions();
        let bob_key_package =
            key_package_with_event_id(bob.fresh_key_package().await.unwrap(), 0xB1);
        let created = {
            let mut alice = alice.lock().await;
            alice
                .create_group(CreateGroupRequest {
                    name: "coordinator failure".to_string(),
                    description: String::new(),
                    members: vec![bob_key_package],
                    required_features: Vec::new(),
                    app_components: vec![nostr_routing_component(b"coordinator-failure")],
                    initial_admins: Vec::new(),
                })
                .await
                .unwrap()
        };
        let pending = pending_from_group_created(&created.effects.publish);
        let publisher = RecordingPublisher::new(
            alice.clone(),
            [MarmotPublishOutcome::Failed {
                reason: "relay rejected welcome".to_string(),
            }],
        );

        let output = publish_effects(alice.clone(), &publisher, created.effects)
            .await
            .unwrap();

        assert_eq!(publisher.published_count().await, 1);
        assert_eq!(
            output.pending,
            vec![MarmotPendingResolution::RolledBack { pending }]
        );
        assert_eq!(output.failures.len(), 1);
        assert!(
            output
                .events
                .iter()
                .all(|event| !matches!(event, GroupEvent::GroupCreated { .. }))
        );
    }

    struct RecordingPublisher {
        session: Arc<Mutex<MarmotSession>>,
        outcomes: Mutex<VecDeque<MarmotPublishOutcome>>,
        published: Mutex<Vec<TransportMessage>>,
        lock_was_available: AtomicBool,
    }

    impl RecordingPublisher {
        fn new(
            session: Arc<Mutex<MarmotSession>>,
            outcomes: impl IntoIterator<Item = MarmotPublishOutcome>,
        ) -> Self {
            Self {
                session,
                outcomes: Mutex::new(outcomes.into_iter().collect()),
                published: Mutex::new(Vec::new()),
                lock_was_available: AtomicBool::new(false),
            }
        }

        fn lock_was_available_during_publish(&self) -> bool {
            self.lock_was_available.load(Ordering::SeqCst)
        }

        async fn published_count(&self) -> usize {
            self.published.lock().await.len()
        }
    }

    #[async_trait]
    impl MarmotMessagePublisher for RecordingPublisher {
        async fn publish(&self, message: TransportMessage) -> MarmotPublishOutcome {
            if self.session.try_lock().is_ok() {
                self.lock_was_available.store(true, Ordering::SeqCst);
            }
            self.published.lock().await.push(message);
            self.outcomes
                .lock()
                .await
                .pop_front()
                .unwrap_or(MarmotPublishOutcome::Published { accepted_count: 1 })
        }
    }

    fn local_sessions() -> (Arc<Mutex<MarmotSession>>, MarmotSession) {
        let alice_keys = Keys::generate();
        let bob_keys = Keys::generate();
        let alice = MarmotSession::open_local(
            alice_keys.public_key(),
            WhitenoiseMarmotStorage::in_memory().unwrap(),
            alice_keys,
        )
        .unwrap();
        let bob = MarmotSession::open_local(
            bob_keys.public_key(),
            WhitenoiseMarmotStorage::in_memory().unwrap(),
            bob_keys,
        )
        .unwrap();

        (Arc::new(Mutex::new(alice)), bob)
    }

    fn pending_from_group_created(
        work: &[PublishWork],
    ) -> cgka_traits::engine_state::PendingStateRef {
        let PublishWork::GroupCreated { pending, .. } = &work[0] else {
            panic!("create_group must produce GroupCreated publish work");
        };
        *pending
    }

    fn key_package_with_event_id(key_package: KeyPackage, marker: u8) -> KeyPackage {
        KeyPackage::with_source_event_id(
            key_package.bytes().to_vec(),
            MessageId::new(vec![marker; 32]),
        )
    }

    fn nostr_routing_component(seed: &[u8]) -> AppComponentData {
        let routing = NostrRoutingV1::new(
            deterministic_nostr_group_id(seed),
            vec!["wss://group.example".to_string()],
        )
        .unwrap();
        AppComponentData {
            component_id: NOSTR_ROUTING_COMPONENT_ID,
            data: encode_nostr_routing_v1(&routing).unwrap(),
        }
    }

    fn deterministic_nostr_group_id(seed: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"whitenoise-marmot-publish-test-nostr-group-id-v1");
        hasher.update(seed);
        hasher.finalize().into()
    }
}

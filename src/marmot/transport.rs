use std::collections::HashMap;
use std::fmt::Display;
use std::sync::Arc;

use async_trait::async_trait;
use cgka_traits::{
    GroupId, MemberId, NostrRoutingV1, TransportAdapter, TransportEndpoint, TransportEnvelope,
    TransportGroupSubscription, TransportMessage, TransportPublishRequest, TransportPublishTarget,
};
use nostr_sdk::prelude::{Event, EventId, Output, RelayUrl};
use thiserror::Error;
use transport_nostr_peeler::NostrTransportEvent;

use super::publish::{MarmotMessagePublisher, MarmotPublishOutcome};

#[derive(Clone)]
pub(crate) struct MarmotTransportPublisher {
    account_id: MemberId,
    adapter: Arc<dyn TransportAdapter>,
    routes: MarmotPublishRoutes,
}

#[cfg_attr(not(test), expect(dead_code, reason = "phase 6"))]
impl MarmotTransportPublisher {
    pub(crate) fn new(
        account_id: MemberId,
        adapter: Arc<dyn TransportAdapter>,
        routes: MarmotPublishRoutes,
    ) -> Self {
        Self {
            account_id,
            adapter,
            routes,
        }
    }
}

#[async_trait]
impl MarmotMessagePublisher for MarmotTransportPublisher {
    async fn publish(&self, message: TransportMessage) -> MarmotPublishOutcome {
        let target = match self.routes.publish_target(&message) {
            Ok(target) => target,
            Err(error) => {
                return MarmotPublishOutcome::Failed {
                    reason: error.to_string(),
                };
            }
        };
        let request = TransportPublishRequest {
            account_id: self.account_id.clone(),
            message,
            target,
            required_acks: self.routes.required_acks,
        };
        match self.adapter.publish(request).await {
            Ok(report) if report.met_required_acks() => MarmotPublishOutcome::Published {
                accepted_count: report.accepted_count(),
            },
            Ok(report) => MarmotPublishOutcome::Failed {
                reason: format!(
                    "insufficient publish acknowledgements: accepted {}/{}",
                    report.accepted_count(),
                    report.required_acks
                ),
            },
            Err(error) => MarmotPublishOutcome::Failed {
                reason: error.to_string(),
            },
        }
    }
}

#[async_trait]
pub(crate) trait MarmotSignedEventPublisher {
    type Error: Display + Send + Sync;

    async fn publish_signed_event(
        &self,
        event: Event,
        relays: &[RelayUrl],
        required_acks: usize,
    ) -> core::result::Result<Output<EventId>, Self::Error>;
}

pub(crate) struct MarmotRelayControlPublisher<'a, P>
where
    P: MarmotSignedEventPublisher + Sync + ?Sized,
{
    signed_event_publisher: &'a P,
    routes: MarmotPublishRoutes,
}

impl<'a, P> MarmotRelayControlPublisher<'a, P>
where
    P: MarmotSignedEventPublisher + Sync + ?Sized,
{
    pub(crate) fn new(signed_event_publisher: &'a P, routes: MarmotPublishRoutes) -> Self {
        Self {
            signed_event_publisher,
            routes,
        }
    }
}

#[async_trait]
impl<P> MarmotMessagePublisher for MarmotRelayControlPublisher<'_, P>
where
    P: MarmotSignedEventPublisher + Sync + ?Sized,
{
    async fn publish(&self, message: TransportMessage) -> MarmotPublishOutcome {
        let target = match self.routes.publish_target(&message) {
            Ok(target) => target,
            Err(error) => {
                return MarmotPublishOutcome::Failed {
                    reason: error.to_string(),
                };
            }
        };
        let event = match signed_nostr_event_from_transport_message(&message) {
            Ok(event) => event,
            Err(error) => {
                return MarmotPublishOutcome::Failed {
                    reason: error.to_string(),
                };
            }
        };
        let relays = match relay_urls_from_transport_endpoints(target.endpoints()) {
            Ok(relays) => relays,
            Err(error) => {
                return MarmotPublishOutcome::Failed {
                    reason: error.to_string(),
                };
            }
        };

        match self
            .signed_event_publisher
            .publish_signed_event(event, &relays, self.routes.required_acks)
            .await
        {
            Ok(output) if output.success.len() >= self.routes.required_acks => {
                MarmotPublishOutcome::Published {
                    accepted_count: output.success.len(),
                }
            }
            Ok(output) => MarmotPublishOutcome::Failed {
                reason: format!(
                    "insufficient publish acknowledgements: accepted {}/{}",
                    output.success.len(),
                    self.routes.required_acks
                ),
            },
            Err(error) => MarmotPublishOutcome::Failed {
                reason: error.to_string(),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MarmotPublishRoutes {
    group_routes: HashMap<Vec<u8>, MarmotGroupPublishRoute>,
    inbox_routes: HashMap<MemberId, Vec<TransportEndpoint>>,
    required_acks: usize,
}

#[cfg_attr(not(test), expect(dead_code, reason = "phase 6"))]
impl MarmotPublishRoutes {
    pub(crate) fn new() -> Self {
        Self {
            group_routes: HashMap::new(),
            inbox_routes: HashMap::new(),
            required_acks: 1,
        }
    }

    pub(crate) fn with_group_route(
        mut self,
        group_id: GroupId,
        transport_group_id: Vec<u8>,
        endpoints: Vec<TransportEndpoint>,
    ) -> Self {
        self = self.with_group_publish_route(MarmotGroupPublishRoute {
            group_id,
            transport_group_id,
            endpoints,
        });
        self
    }

    pub(crate) fn with_group_publish_route(mut self, route: MarmotGroupPublishRoute) -> Self {
        self.group_routes
            .insert(route.transport_group_id.clone(), route);
        self
    }

    pub(crate) fn with_inbox_route(
        mut self,
        recipient: MemberId,
        endpoints: Vec<TransportEndpoint>,
    ) -> Self {
        self.inbox_routes.insert(recipient, endpoints);
        self
    }

    pub(crate) fn with_required_acks(mut self, required_acks: usize) -> Self {
        self.required_acks = required_acks;
        self
    }

    fn publish_target(
        &self,
        message: &TransportMessage,
    ) -> core::result::Result<TransportPublishTarget, MarmotTransportRouteError> {
        match &message.envelope {
            TransportEnvelope::GroupMessage { transport_group_id } => {
                let route = self.group_routes.get(transport_group_id).ok_or_else(|| {
                    MarmotTransportRouteError::MissingGroup {
                        transport_group_id: hex::encode(transport_group_id),
                    }
                })?;
                if route.endpoints.is_empty() {
                    return Err(MarmotTransportRouteError::EmptyGroup {
                        transport_group_id: hex::encode(transport_group_id),
                    });
                }
                Ok(TransportPublishTarget::Group {
                    group_id: route.group_id.clone(),
                    transport_group_id: route.transport_group_id.clone(),
                    endpoints: route.endpoints.clone(),
                })
            }
            TransportEnvelope::Welcome { recipient } => {
                let endpoints = self.inbox_routes.get(recipient).ok_or_else(|| {
                    MarmotTransportRouteError::MissingInbox {
                        recipient: recipient.clone(),
                    }
                })?;
                if endpoints.is_empty() {
                    return Err(MarmotTransportRouteError::EmptyInbox {
                        recipient: recipient.clone(),
                    });
                }
                Ok(TransportPublishTarget::Inbox {
                    recipient: recipient.clone(),
                    endpoints: endpoints.clone(),
                })
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MarmotGroupPublishRoute {
    pub(crate) group_id: GroupId,
    pub(crate) transport_group_id: Vec<u8>,
    pub(crate) endpoints: Vec<TransportEndpoint>,
}

impl MarmotGroupPublishRoute {
    pub(crate) fn from_nostr_routing(group_id: GroupId, routing: NostrRoutingV1) -> Self {
        Self {
            group_id,
            transport_group_id: routing.nostr_group_id.to_vec(),
            endpoints: routing.relays.into_iter().map(TransportEndpoint).collect(),
        }
    }

    pub(crate) fn into_group_subscription(self) -> TransportGroupSubscription {
        TransportGroupSubscription {
            group_id: self.group_id,
            transport_group_id: self.transport_group_id,
            endpoints: self.endpoints,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
enum MarmotTransportRouteError {
    #[error("missing Marmot group publish route for transport group {transport_group_id}")]
    MissingGroup { transport_group_id: String },

    #[error("Marmot group publish route has no endpoints for transport group {transport_group_id}")]
    EmptyGroup { transport_group_id: String },

    #[error("missing Marmot inbox publish route for recipient {recipient}")]
    MissingInbox { recipient: MemberId },

    #[error("Marmot inbox publish route has no endpoints for recipient {recipient}")]
    EmptyInbox { recipient: MemberId },
}

fn signed_nostr_event_from_transport_message(
    message: &TransportMessage,
) -> core::result::Result<Event, MarmotNostrEventError> {
    let event = NostrTransportEvent::from_transport_message(message).map_err(|error| {
        MarmotNostrEventError::TransportPayload {
            reason: error.to_string(),
        }
    })?;
    let signed =
        event
            .to_verified_nostr_event()
            .map_err(|error| MarmotNostrEventError::SignedEvent {
                reason: error.to_string(),
            })?;
    if signed.id.to_bytes().as_slice() != message.id.as_slice() {
        return Err(MarmotNostrEventError::MessageIdMismatch {
            event_id: signed.id.to_hex(),
            message_id: hex::encode(message.id.as_slice()),
        });
    }

    Ok(signed)
}

fn relay_urls_from_transport_endpoints(
    endpoints: &[TransportEndpoint],
) -> core::result::Result<Vec<RelayUrl>, MarmotNostrEventError> {
    endpoints
        .iter()
        .map(|endpoint| {
            RelayUrl::parse(&endpoint.0).map_err(|error| MarmotNostrEventError::RelayEndpoint {
                endpoint: endpoint.0.clone(),
                reason: error.to_string(),
            })
        })
        .collect()
}

#[derive(Debug, Error, PartialEq, Eq)]
enum MarmotNostrEventError {
    #[error("invalid Marmot Nostr transport payload: {reason}")]
    TransportPayload { reason: String },

    #[error("invalid signed Marmot Nostr event: {reason}")]
    SignedEvent { reason: String },

    #[error("Marmot Nostr event id {event_id} does not match transport message id {message_id}")]
    MessageIdMismatch {
        event_id: String,
        message_id: String,
    },

    #[error("invalid Marmot relay endpoint {endpoint}: {reason}")]
    RelayEndpoint { endpoint: String, reason: String },
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use cgka_traits::{
        GroupId, MemberId, MessageId, Timestamp, TransportAccountActivation, TransportAdapter,
        TransportAdapterError, TransportDelivery, TransportEndpoint, TransportEndpointReceipt,
        TransportEnvelope, TransportGroupSync, TransportMessage, TransportPublishReport,
        TransportPublishRequest, TransportPublishTarget, TransportSource,
    };

    use nostr_sdk::prelude::Output;
    use nostr_sdk::{Event, EventBuilder, EventId, Keys, Kind, RelayUrl, Tag, TagKind};
    use transport_nostr_peeler::{KIND_MARMOT_GROUP_MESSAGE, NostrTransportEvent};

    use super::{
        MarmotPublishRoutes, MarmotRelayControlPublisher, MarmotSignedEventPublisher,
        MarmotTransportPublisher,
    };
    use crate::marmot::publish::{MarmotMessagePublisher, MarmotPublishOutcome};

    #[tokio::test]
    async fn group_message_publishes_to_resolved_group_target() {
        let account_id = MemberId::new(vec![0xA1; 32]);
        let group_id = GroupId::new(vec![0xB2; 32]);
        let transport_group_id = vec![0xC3; 32];
        let endpoints = vec![
            TransportEndpoint("wss://one.example".to_string()),
            TransportEndpoint("wss://two.example".to_string()),
        ];
        let routes = MarmotPublishRoutes::new()
            .with_group_route(
                group_id.clone(),
                transport_group_id.clone(),
                endpoints.clone(),
            )
            .with_required_acks(2);
        let adapter = Arc::new(RecordingTransportAdapter::accepted(2));
        let publisher = MarmotTransportPublisher::new(account_id.clone(), adapter.clone(), routes);

        let outcome = publisher
            .publish(group_message(transport_group_id.clone()))
            .await;

        assert_eq!(
            outcome,
            MarmotPublishOutcome::Published { accepted_count: 2 }
        );
        let requests = adapter.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].account_id, account_id);
        assert_eq!(requests[0].required_acks, 2);
        assert_eq!(
            requests[0].target,
            TransportPublishTarget::Group {
                group_id,
                transport_group_id,
                endpoints,
            }
        );
    }

    #[tokio::test]
    async fn welcome_message_publishes_to_resolved_inbox_target() {
        let account_id = MemberId::new(vec![0xA1; 32]);
        let recipient = MemberId::new(vec![0xB2; 32]);
        let endpoints = vec![TransportEndpoint("wss://inbox.example".to_string())];
        let routes =
            MarmotPublishRoutes::new().with_inbox_route(recipient.clone(), endpoints.clone());
        let adapter = Arc::new(RecordingTransportAdapter::accepted(1));
        let publisher = MarmotTransportPublisher::new(account_id.clone(), adapter.clone(), routes);

        let outcome = publisher.publish(welcome_message(recipient.clone())).await;

        assert_eq!(
            outcome,
            MarmotPublishOutcome::Published { accepted_count: 1 }
        );
        let requests = adapter.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].account_id, account_id);
        assert_eq!(
            requests[0].target,
            TransportPublishTarget::Inbox {
                recipient,
                endpoints,
            }
        );
    }

    #[tokio::test]
    async fn publish_fails_when_adapter_does_not_meet_required_acks() {
        let account_id = MemberId::new(vec![0xA1; 32]);
        let group_id = GroupId::new(vec![0xB2; 32]);
        let transport_group_id = vec![0xC3; 32];
        let endpoints = vec![
            TransportEndpoint("wss://one.example".to_string()),
            TransportEndpoint("wss://two.example".to_string()),
        ];
        let routes = MarmotPublishRoutes::new()
            .with_group_route(group_id, transport_group_id.clone(), endpoints)
            .with_required_acks(2);
        let adapter = Arc::new(RecordingTransportAdapter::accepted(1));
        let publisher = MarmotTransportPublisher::new(account_id, adapter.clone(), routes);

        let outcome = publisher.publish(group_message(transport_group_id)).await;

        assert_eq!(
            outcome,
            MarmotPublishOutcome::Failed {
                reason: "insufficient publish acknowledgements: accepted 1/2".to_string(),
            }
        );
        assert_eq!(adapter.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn relay_control_publisher_verifies_signed_event_and_uses_required_acks() {
        let group_id = GroupId::new(vec![0xB2; 32]);
        let transport_group_id = vec![0xC3; 32];
        let endpoints = vec![
            TransportEndpoint("wss://one.example".to_string()),
            TransportEndpoint("wss://two.example".to_string()),
        ];
        let routes = MarmotPublishRoutes::new()
            .with_group_route(group_id, transport_group_id.clone(), endpoints)
            .with_required_acks(2);
        let backend = RecordingSignedEventPublisher::accepted(2);
        let publisher = MarmotRelayControlPublisher::new(&backend, routes);

        let outcome = publisher
            .publish(signed_group_message(transport_group_id))
            .await;

        assert_eq!(
            outcome,
            MarmotPublishOutcome::Published { accepted_count: 2 }
        );
        let requests = backend.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].relays.len(), 2);
        assert_eq!(
            requests[0].relays[0],
            RelayUrl::parse("wss://one.example").unwrap()
        );
        assert_eq!(
            requests[0].relays[1],
            RelayUrl::parse("wss://two.example").unwrap()
        );
        assert_eq!(requests[0].required_acks, 2);
        assert_eq!(requests[0].event.id.to_bytes(), requests[0].message_id);
    }

    #[tokio::test]
    async fn relay_control_publisher_rejects_unsigned_transport_events_before_publish() {
        let group_id = GroupId::new(vec![0xB2; 32]);
        let transport_group_id = vec![0xC3; 32];
        let routes = MarmotPublishRoutes::new().with_group_route(
            group_id,
            transport_group_id.clone(),
            vec![TransportEndpoint("wss://relay.example".to_string())],
        );
        let backend = RecordingSignedEventPublisher::accepted(1);
        let publisher = MarmotRelayControlPublisher::new(&backend, routes);

        let outcome = publisher
            .publish(unsigned_group_message(transport_group_id))
            .await;

        let MarmotPublishOutcome::Failed { reason } = outcome else {
            panic!("unsigned transport event must fail");
        };
        assert!(reason.contains("signed Nostr event is missing sig"));
        assert!(backend.requests.lock().unwrap().is_empty());
    }

    struct RecordingTransportAdapter {
        requests: Mutex<Vec<TransportPublishRequest>>,
        accepted_count: usize,
    }

    impl RecordingTransportAdapter {
        fn accepted(accepted_count: usize) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                accepted_count,
            }
        }
    }

    #[async_trait]
    impl TransportAdapter for RecordingTransportAdapter {
        async fn activate_account(
            &self,
            _activation: TransportAccountActivation,
        ) -> Result<(), TransportAdapterError> {
            Ok(())
        }

        async fn sync_account_groups(
            &self,
            _sync: TransportGroupSync,
        ) -> Result<(), TransportAdapterError> {
            Ok(())
        }

        async fn deactivate_account(
            &self,
            _account_id: &MemberId,
        ) -> Result<(), TransportAdapterError> {
            Ok(())
        }

        async fn publish(
            &self,
            request: TransportPublishRequest,
        ) -> Result<TransportPublishReport, TransportAdapterError> {
            let message_id = request.message.id.clone();
            let accepted = request
                .target
                .endpoints()
                .iter()
                .take(self.accepted_count)
                .cloned()
                .map(|endpoint| TransportEndpointReceipt {
                    endpoint,
                    accepted_at: None,
                })
                .collect();
            let required_acks = request.required_acks;
            self.requests.lock().unwrap().push(request);
            Ok(TransportPublishReport {
                message_id,
                accepted,
                failed: Vec::new(),
                required_acks,
            })
        }

        async fn receive(&self) -> Result<Option<TransportDelivery>, TransportAdapterError> {
            Ok(None)
        }
    }

    fn group_message(transport_group_id: Vec<u8>) -> TransportMessage {
        TransportMessage {
            id: MessageId::new(vec![0xD4; 32]),
            payload: Vec::new(),
            timestamp: Timestamp(1),
            causal_deps: Vec::new(),
            source: TransportSource("nostr".to_string()),
            envelope: TransportEnvelope::GroupMessage { transport_group_id },
        }
    }

    fn welcome_message(recipient: MemberId) -> TransportMessage {
        TransportMessage {
            id: MessageId::new(vec![0xE5; 32]),
            payload: Vec::new(),
            timestamp: Timestamp(1),
            causal_deps: Vec::new(),
            source: TransportSource("nostr".to_string()),
            envelope: TransportEnvelope::Welcome { recipient },
        }
    }

    struct RecordingSignedEventPublisher {
        requests: Mutex<Vec<SignedEventPublishRequest>>,
        accepted_count: usize,
    }

    struct SignedEventPublishRequest {
        message_id: [u8; 32],
        event: Event,
        relays: Vec<RelayUrl>,
        required_acks: usize,
    }

    impl RecordingSignedEventPublisher {
        fn accepted(accepted_count: usize) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                accepted_count,
            }
        }
    }

    #[async_trait]
    impl MarmotSignedEventPublisher for RecordingSignedEventPublisher {
        type Error = String;

        async fn publish_signed_event(
            &self,
            event: Event,
            relays: &[RelayUrl],
            required_acks: usize,
        ) -> Result<Output<EventId>, Self::Error> {
            self.requests
                .lock()
                .unwrap()
                .push(SignedEventPublishRequest {
                    message_id: event.id.to_bytes(),
                    event: event.clone(),
                    relays: relays.to_vec(),
                    required_acks,
                });
            Ok(Output {
                val: event.id,
                success: relays.iter().take(self.accepted_count).cloned().collect(),
                failed: HashMap::new(),
            })
        }
    }

    fn signed_group_message(transport_group_id: Vec<u8>) -> TransportMessage {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(KIND_MARMOT_GROUP_MESSAGE as u16), "payload")
            .tags([Tag::custom(
                TagKind::custom("h"),
                [hex::encode(transport_group_id)],
            )])
            .sign_with_keys(&keys)
            .unwrap();
        let transport_event = NostrTransportEvent::from_nostr_event(&event).unwrap();
        transport_event.to_transport_message().unwrap()
    }

    fn unsigned_group_message(transport_group_id: Vec<u8>) -> TransportMessage {
        let event = NostrTransportEvent {
            id: "11".repeat(32),
            pubkey: "22".repeat(32),
            created_at: 1_700_000_000,
            kind: KIND_MARMOT_GROUP_MESSAGE,
            tags: vec![vec!["h".to_string(), hex::encode(transport_group_id)]],
            content: "payload".to_string(),
            sig: None,
        };
        event.to_transport_message().unwrap()
    }
}

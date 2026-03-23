use nostr_sdk::{RelayMessage, RelayUrl};

use crate::relay_control::observability::RelayFailureCategory;

/// Normalized relay notification surface for future `RelaySession` wiring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelayNotification {
    Notice {
        relay_url: RelayUrl,
        message: String,
        failure_category: Option<RelayFailureCategory>,
    },
    Closed {
        relay_url: RelayUrl,
        message: String,
        failure_category: Option<RelayFailureCategory>,
    },
    Auth {
        relay_url: RelayUrl,
        challenge: String,
        failure_category: Option<RelayFailureCategory>,
    },
    #[allow(dead_code)]
    Connected { relay_url: RelayUrl },
    #[allow(dead_code)]
    Disconnected {
        relay_url: RelayUrl,
        failure_category: Option<RelayFailureCategory>,
    },
    #[allow(dead_code)]
    Shutdown,
}

impl RelayNotification {
    pub(crate) fn from_message(
        relay_url: RelayUrl,
        message: RelayMessage<'static>,
    ) -> Option<Self> {
        // Only actionable relay messages (Notice, Closed, Auth) produce
        // notifications. High-volume happy-path messages (OK, EOSE, Count,
        // NegMsg, NegErr) are filtered out to avoid polluting the
        // relay_events table with non-actionable data.
        match message {
            // Events are handled by the RelayPoolNotification::Event branch
            // in the pool notification loop, not through this path.
            RelayMessage::Event { .. } => None,
            RelayMessage::Ok { .. }
            | RelayMessage::EndOfStoredEvents(_)
            | RelayMessage::Count { .. }
            | RelayMessage::NegMsg { .. }
            | RelayMessage::NegErr { .. } => None,
            RelayMessage::Notice(message) => Some(Self::Notice {
                relay_url,
                failure_category: Some(RelayFailureCategory::classify_notice(&message)),
                message: message.into_owned(),
            }),
            RelayMessage::Closed {
                subscription_id: _,
                message,
            } => Some(Self::Closed {
                relay_url,
                failure_category: Some(RelayFailureCategory::classify_closed(&message)),
                message: message.into_owned(),
            }),
            RelayMessage::Auth { challenge } => Some(Self::Auth {
                relay_url,
                failure_category: Some(RelayFailureCategory::classify_auth(&challenge)),
                challenge: challenge.into_owned(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use nostr_sdk::{EventId, RelayMessage, RelayUrl, SubscriptionId};

    use super::*;

    fn test_relay_url() -> RelayUrl {
        RelayUrl::parse("wss://relay.example.com").unwrap()
    }

    #[test]
    fn non_actionable_messages_are_filtered() {
        let sub_id = SubscriptionId::new("sub1");
        let cases: Vec<RelayMessage<'static>> = vec![
            RelayMessage::ok(EventId::all_zeros(), true, ""),
            RelayMessage::eose(sub_id.clone()),
            RelayMessage::count(sub_id.clone(), 42),
            RelayMessage::NegMsg {
                subscription_id: Cow::Owned(sub_id.clone()),
                message: Cow::Owned("abc".to_string()),
            },
            RelayMessage::NegErr {
                subscription_id: Cow::Owned(sub_id),
                message: Cow::Owned("error".to_string()),
            },
        ];
        for message in cases {
            assert_eq!(
                RelayNotification::from_message(test_relay_url(), message),
                None
            );
        }
    }

    #[test]
    fn notice_message_produces_notification() {
        let result = RelayNotification::from_message(
            test_relay_url(),
            RelayMessage::notice("something went wrong"),
        );
        assert!(result.is_some());
        match result.unwrap() {
            RelayNotification::Notice { message, .. } => {
                assert_eq!(message, "something went wrong");
            }
            other => panic!("expected Notice, got {:?}", other),
        }
    }

    #[test]
    fn closed_message_produces_notification() {
        let result = RelayNotification::from_message(
            test_relay_url(),
            RelayMessage::closed(SubscriptionId::new("sub1"), "rate-limited:"),
        );
        assert!(result.is_some());
        match result.unwrap() {
            RelayNotification::Closed { message, .. } => {
                assert_eq!(message, "rate-limited:");
            }
            other => panic!("expected Closed, got {:?}", other),
        }
    }

    #[test]
    fn auth_message_produces_notification() {
        let result =
            RelayNotification::from_message(test_relay_url(), RelayMessage::auth("challenge123"));
        assert!(result.is_some());
        match result.unwrap() {
            RelayNotification::Auth { challenge, .. } => {
                assert_eq!(challenge, "challenge123");
            }
            other => panic!("expected Auth, got {:?}", other),
        }
    }
}

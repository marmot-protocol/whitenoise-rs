use core::fmt;
use std::cmp::Ordering;
use std::str::FromStr;

use cgka_traits::app_event::MarmotAppEvent;
use cgka_traits::types::{MemberId, MessageId};
use nostr_sdk::prelude::{EventId, Kind, PublicKey, Tag, Tags, Timestamp, UnsignedEvent};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::marmot::GroupId;
use crate::whitenoise::error::WhitenoiseError;

/// Public WhiteNoise projection of an application message inside a Marmot group.
///
/// The field shape intentionally stays stable for callers while keeping
/// upstream engine types out of caller-facing APIs.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Message {
    /// Event id of the inner unsigned app message.
    pub id: EventId,
    /// Author public key of the inner app message.
    pub pubkey: PublicKey,
    /// Nostr kind of the inner app message.
    pub kind: Kind,
    /// MLS group id for this message.
    pub mls_group_id: GroupId,
    /// Inner event creation timestamp.
    pub created_at: Timestamp,
    /// Local processing timestamp.
    pub processed_at: Timestamp,
    /// Inner event content.
    pub content: String,
    /// Inner event tags.
    pub tags: Tags,
    /// Unsigned inner Nostr event.
    pub event: UnsignedEvent,
    /// Wrapper event id that carried the message.
    pub wrapper_event_id: EventId,
    /// Group epoch that processed this message, when known.
    pub epoch: Option<u64>,
    /// Message lifecycle state.
    pub state: MessageState,
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message").finish_non_exhaustive()
    }
}

impl Message {
    pub(crate) fn from_unsigned_app_event(
        group_id: GroupId,
        mut event: UnsignedEvent,
        wrapper_event_id: EventId,
        epoch: Option<u64>,
        state: MessageState,
    ) -> crate::whitenoise::error::Result<Self> {
        event.ensure_id();
        let id = event.id.ok_or_else(|| {
            WhitenoiseError::Internal("unsigned Marmot app event is missing its id".to_string())
        })?;

        Ok(Self {
            id,
            pubkey: event.pubkey,
            kind: event.kind,
            mls_group_id: group_id,
            created_at: event.created_at,
            processed_at: Timestamp::now(),
            content: event.content.clone(),
            tags: event.tags.clone(),
            event,
            wrapper_event_id,
            epoch,
            state,
        })
    }

    pub(crate) fn from_app_payload(
        group_id: GroupId,
        payload: &[u8],
        wrapper_event_id: EventId,
        epoch: Option<u64>,
        state: MessageState,
        authenticated_sender: &MemberId,
    ) -> crate::whitenoise::error::Result<Self> {
        let app_event = MarmotAppEvent::decode(payload).map_err(|error| {
            WhitenoiseError::InvalidInput(format!("invalid Marmot app event payload: {error}"))
        })?;
        let sender_pubkey =
            PublicKey::from_slice(authenticated_sender.as_slice()).map_err(|error| {
                WhitenoiseError::InvalidInput(format!(
                    "invalid Marmot authenticated sender pubkey: {error}"
                ))
            })?;
        app_event
            .validate_sender(&sender_pubkey.to_hex())
            .map_err(|error| {
                WhitenoiseError::InvalidInput(format!("Marmot app event sender mismatch: {error}"))
            })?;

        let id = EventId::from_hex(&app_event.id).map_err(|error| {
            WhitenoiseError::InvalidInput(format!("invalid Marmot app event id: {error}"))
        })?;
        let pubkey = PublicKey::from_hex(&app_event.pubkey).map_err(|error| {
            WhitenoiseError::InvalidInput(format!("invalid Marmot app event pubkey: {error}"))
        })?;
        let kind = Kind::from(u16::try_from(app_event.kind).map_err(|_| {
            WhitenoiseError::InvalidInput(format!(
                "Marmot app event kind {} exceeds Nostr u16 kind range",
                app_event.kind
            ))
        })?);
        let tags = Tags::from_list(
            app_event
                .tags
                .iter()
                .map(|tag| {
                    Tag::parse(tag.iter().map(String::as_str)).map_err(WhitenoiseError::from)
                })
                .collect::<crate::whitenoise::error::Result<Vec<_>>>()?,
        );
        let created_at = Timestamp::from(app_event.created_at);
        let event = UnsignedEvent {
            id: Some(id),
            pubkey,
            created_at,
            kind,
            tags: tags.clone(),
            content: app_event.content.clone(),
        };

        Ok(Self {
            id,
            pubkey,
            kind,
            mls_group_id: group_id,
            created_at,
            processed_at: Timestamp::now(),
            content: app_event.content,
            tags,
            event,
            wrapper_event_id,
            epoch,
            state,
        })
    }

    /// Compares two messages for display ordering.
    ///
    /// Messages are sorted newest-first by `created_at`, then `processed_at`,
    /// then `id`.
    pub fn display_order_cmp(&self, other: &Self) -> Ordering {
        Self::compare_display_keys(
            self.created_at,
            self.processed_at,
            self.id,
            other.created_at,
            other.processed_at,
            other.id,
        )
    }

    /// Compares display ordering keys without requiring full message values.
    pub fn compare_display_keys(
        a_created_at: Timestamp,
        a_processed_at: Timestamp,
        a_id: EventId,
        b_created_at: Timestamp,
        b_processed_at: Timestamp,
        b_id: EventId,
    ) -> Ordering {
        a_created_at
            .cmp(&b_created_at)
            .then_with(|| a_processed_at.cmp(&b_processed_at))
            .then_with(|| a_id.cmp(&b_id))
    }

    /// Compares two messages for processed-at-first ordering.
    pub fn processed_at_order_cmp(&self, other: &Self) -> Ordering {
        Self::compare_processed_at_keys(
            self.processed_at,
            self.created_at,
            self.id,
            other.processed_at,
            other.created_at,
            other.id,
        )
    }

    /// Compares processed-at-first ordering keys without requiring full message values.
    pub fn compare_processed_at_keys(
        a_processed_at: Timestamp,
        a_created_at: Timestamp,
        a_id: EventId,
        b_processed_at: Timestamp,
        b_created_at: Timestamp,
        b_id: EventId,
    ) -> Ordering {
        a_processed_at
            .cmp(&b_processed_at)
            .then_with(|| a_created_at.cmp(&b_created_at))
            .then_with(|| a_id.cmp(&b_id))
    }
}

pub(crate) fn app_payload_from_unsigned_event(
    inner_event: &UnsignedEvent,
    expected_event_id: EventId,
) -> crate::whitenoise::error::Result<Vec<u8>> {
    let tags = inner_event
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .collect();
    let app_event = MarmotAppEvent::new(
        inner_event.pubkey.to_hex(),
        inner_event.created_at.as_secs(),
        u64::from(inner_event.kind.as_u16()),
        tags,
        inner_event.content.clone(),
    );

    if app_event.id != expected_event_id.to_hex() {
        return Err(WhitenoiseError::Internal(format!(
            "Marmot app event id mismatch: expected {}, computed {}",
            expected_event_id.to_hex(),
            app_event.id
        )));
    }

    app_event
        .encode()
        .map_err(|error| WhitenoiseError::InvalidInput(error.to_string()))
}

pub(crate) fn event_id_from_message_id(
    message_id: &MessageId,
) -> crate::whitenoise::error::Result<EventId> {
    EventId::from_slice(message_id.as_slice()).map_err(|error| {
        WhitenoiseError::Internal(format!(
            "Marmot transport message id is not a Nostr event id: {error}"
        ))
    })
}

/// Public lifecycle state for a projected Marmot app message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageState {
    /// The message was created locally and stored before relay publish was known.
    Created,
    /// The message was processed and stored.
    Processed,
    /// The message was deleted by its author.
    Deleted,
    /// The epoch rolled back; content may need reprocessing.
    EpochInvalidated,
}

impl fmt::Display for MessageState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Created => "created",
            Self::Processed => "processed",
            Self::Deleted => "deleted",
            Self::EpochInvalidated => "epoch_invalidated",
        })
    }
}

impl FromStr for MessageState {
    type Err = MessageStateParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "created" => Ok(Self::Created),
            "processed" => Ok(Self::Processed),
            "deleted" => Ok(Self::Deleted),
            "epoch_invalidated" => Ok(Self::EpochInvalidated),
            other => Err(MessageStateParseError {
                value: other.to_string(),
            }),
        }
    }
}

/// Error returned when parsing a public message state string fails.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("Invalid message state: {value}")]
pub struct MessageStateParseError {
    value: String,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use nostr_sdk::prelude::{EventId, Kind, PublicKey, Tag, Tags, Timestamp, UnsignedEvent};

    use super::{Message, MessageState};
    use crate::marmot::GroupId;
    use crate::types::MessageWithTokens;

    #[test]
    fn message_projection_preserves_legacy_public_shape() {
        let message = test_message(MessageState::Processed);

        let serialized = serde_json::to_value(&message).unwrap();

        assert_eq!(
            serialized.get("id").and_then(serde_json::Value::as_str),
            Some(message.id.to_hex().as_str())
        );
        assert_eq!(
            serialized
                .get("content")
                .and_then(serde_json::Value::as_str),
            Some("hello")
        );
        assert_eq!(
            serialized.get("state").and_then(serde_json::Value::as_str),
            Some("processed")
        );
    }

    #[test]
    fn message_with_tokens_uses_marmot_message_projection() {
        let message = test_message(MessageState::Created);
        let tokens = whitenoise_markdown::parse(&message.content);

        let with_tokens = MessageWithTokens::new(message.clone(), tokens);

        assert_eq!(with_tokens.message, message);
        assert_eq!(with_tokens.message.state.to_string(), "created");
    }

    #[test]
    fn message_state_parses_legacy_strings() {
        assert_eq!(
            MessageState::from_str("epoch_invalidated").unwrap(),
            MessageState::EpochInvalidated
        );
        assert!(MessageState::from_str("sent").is_err());
    }

    fn test_message(state: MessageState) -> Message {
        let id = EventId::from_slice(&[1; 32]).unwrap();
        let pubkey = PublicKey::from_slice(&[2; 32]).unwrap();
        let created_at = Timestamp::from(100);
        let kind = Kind::from(9);
        let tags = Tags::from_list(vec![Tag::parse(["p", &pubkey.to_hex()]).unwrap()]);
        let event = UnsignedEvent {
            id: Some(id),
            pubkey,
            created_at,
            kind,
            tags: tags.clone(),
            content: "hello".to_string(),
        };

        Message {
            id,
            pubkey,
            kind,
            mls_group_id: GroupId::from_slice(&[3; 32]),
            created_at,
            processed_at: Timestamp::from(101),
            content: "hello".to_string(),
            tags,
            event,
            wrapper_event_id: EventId::from_slice(&[4; 32]).unwrap(),
            epoch: Some(7),
            state,
        }
    }
}

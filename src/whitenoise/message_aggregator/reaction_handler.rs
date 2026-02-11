//! Reaction-specific processing logic
//!
//! This module handles the processing of reaction messages (kind 7) and manages
//! the aggregation of reactions on target messages.

use nostr_sdk::prelude::*;
use std::collections::HashMap;

use super::emoji_utils;
use super::types::{AggregatorConfig, ChatMessage, EmojiReaction, ProcessingError, UserReaction};
use mdk_core::prelude::message_types::Message;

/// Process a reaction message and update the target message's reaction summary
pub fn process_reaction(
    message: &Message,
    processed_messages: &mut HashMap<String, ChatMessage>,
    config: &AggregatorConfig,
) -> Result<(), ProcessingError> {
    let reaction_emoji =
        emoji_utils::validate_and_normalize_reaction(&message.content, config.normalize_emoji)?;

    let target_id = extract_target_message_id(&message.tags)?;

    if let Some(target_message) = processed_messages.get_mut(&target_id) {
        add_reaction_to_message(
            target_message,
            &message.pubkey,
            &reaction_emoji,
            message.created_at,
            message.id,
        );

        if config.enable_debug_logging {
            tracing::debug!(
                "Added reaction '{}' from {} to message {}",
                reaction_emoji,
                message.pubkey.to_hex(),
                target_id
            );
        }
        Ok(())
    } else {
        if config.enable_debug_logging {
            tracing::warn!(
                "Reaction {} references non-existent message {}",
                message.id,
                target_id
            );
        }
        Err(ProcessingError::Internal(format!(
            "Reaction target {} not found",
            target_id
        )))
    }
}

/// Extract the target message ID from reaction event e-tags
fn extract_target_message_id(tags: &Tags) -> Result<String, ProcessingError> {
    // Look for the first e-tag (target event)
    for tag in tags.iter() {
        if tag.kind() == TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::E))
            && let Some(event_id) = tag.content()
        {
            return Ok(event_id.to_string());
        }
    }
    // Fallback: parameterized replaceable event ("a" tag)
    for tag in tags.iter() {
        if tag.kind() == TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::A))
            && let Some(addr) = tag.content()
        {
            return Ok(addr.to_string());
        }
    }
    Err(ProcessingError::MissingETag)
}

/// Remove a user's reaction from a message's reaction summary
/// Returns true if a reaction was removed, false if user had no reaction
pub(crate) fn remove_reaction_from_message(
    target_message: &mut ChatMessage,
    user: &PublicKey,
) -> bool {
    let Some(idx) = target_message
        .reactions
        .user_reactions
        .iter()
        .position(|ur| ur.user == *user)
    else {
        return false;
    };

    let removed_reaction = target_message.reactions.user_reactions.remove(idx);

    if let Some(emoji_reaction) = target_message
        .reactions
        .by_emoji
        .get_mut(&removed_reaction.emoji)
    {
        emoji_reaction.count = emoji_reaction.count.saturating_sub(1);
        emoji_reaction.users.retain(|u| u != user);

        if emoji_reaction.count == 0 {
            target_message
                .reactions
                .by_emoji
                .remove(&removed_reaction.emoji);
        }
    }

    true
}

/// Add a reaction to a message's reaction summary
/// Assumes the emoji has already been validated and normalized
pub(crate) fn add_reaction_to_message(
    target_message: &mut ChatMessage,
    user: &PublicKey,
    emoji: &str,
    created_at: Timestamp,
    reaction_id: EventId,
) {
    // Remove any existing reaction from this user first (one reaction per user)
    remove_reaction_from_message(target_message, user);

    // Add new reaction
    let user_reaction = UserReaction {
        user: *user,
        emoji: emoji.to_string(),
        created_at,
        reaction_id,
    };

    target_message.reactions.user_reactions.push(user_reaction);

    // Update emoji count
    let emoji_reaction = target_message
        .reactions
        .by_emoji
        .entry(emoji.to_string())
        .or_insert_with(|| EmojiReaction {
            emoji: emoji.to_string(),
            count: 0,
            users: Vec::new(),
        });

    emoji_reaction.count += 1;
    if !emoji_reaction.users.contains(user) {
        emoji_reaction.users.push(*user);
    }

    // Sort user reactions by timestamp for consistent ordering
    target_message
        .reactions
        .user_reactions
        .sort_by(|a, b| a.created_at.cmp(&b.created_at));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::message_aggregator::types::ReactionSummary;
    use mdk_core::prelude::GroupId;
    use mdk_core::prelude::message_types::{Message, MessageState};
    use nostr_sdk::UnsignedEvent;

    fn create_chat_message(id: &str) -> ChatMessage {
        let keys = Keys::generate();

        ChatMessage {
            id: id.to_string(),
            author: keys.public_key(),
            content: "Test message".to_string(),
            created_at: Timestamp::from(1234567890),
            tags: Tags::new(),
            is_reply: false,
            reply_to_id: None,
            is_deleted: false,
            content_tokens: vec![],
            reactions: ReactionSummary::default(),
            kind: 9, // Default to MLS group chat
            media_attachments: vec![],
        }
    }

    #[test]
    fn test_extract_target_message_id() {
        let mut tags = Tags::new();
        tags.push(Tag::parse(vec!["e", "target_msg_id"]).unwrap());
        tags.push(Tag::parse(vec!["p", "some_user"]).unwrap());

        let result = extract_target_message_id(&tags).unwrap();
        assert_eq!(result, "target_msg_id");
    }

    #[test]
    fn test_extract_target_message_id_missing() {
        let mut tags = Tags::new();
        tags.push(Tag::parse(vec!["p", "some_user"]).unwrap());

        let result = extract_target_message_id(&tags);
        assert!(result.is_err());
        assert!(matches!(result, Err(ProcessingError::MissingETag)));
    }

    #[test]
    fn test_extract_target_message_id_empty_tags() {
        let tags = Tags::new();

        let result = extract_target_message_id(&tags);
        assert!(result.is_err());
        assert!(matches!(result, Err(ProcessingError::MissingETag)));
    }

    #[test]
    fn test_add_reaction_to_message() {
        let mut chat_message = create_chat_message("msg1");
        let user = Keys::generate().public_key();
        let created_at = Timestamp::from(1234567890);
        let reaction_id = EventId::all_zeros();

        add_reaction_to_message(&mut chat_message, &user, "👍", created_at, reaction_id);

        // Check user reactions
        assert_eq!(chat_message.reactions.user_reactions.len(), 1);
        assert_eq!(chat_message.reactions.user_reactions[0].user, user);
        assert_eq!(chat_message.reactions.user_reactions[0].emoji, "👍");
        assert_eq!(
            chat_message.reactions.user_reactions[0].reaction_id,
            reaction_id
        );

        // Check emoji aggregation
        assert_eq!(chat_message.reactions.by_emoji.len(), 1);
        let emoji_reaction = chat_message.reactions.by_emoji.get("👍").unwrap();
        assert_eq!(emoji_reaction.count, 1);
        assert_eq!(emoji_reaction.users.len(), 1);
        assert_eq!(emoji_reaction.users[0], user);
    }

    #[test]
    fn test_replace_existing_reaction() {
        let mut chat_message = create_chat_message("msg1");
        let user = Keys::generate().public_key();
        let created_at = Timestamp::from(1234567890);
        let reaction_id1 = EventId::all_zeros();
        let reaction_id2 =
            EventId::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();

        // Add first reaction
        add_reaction_to_message(&mut chat_message, &user, "👍", created_at, reaction_id1);

        // Replace with different reaction
        add_reaction_to_message(&mut chat_message, &user, "❤", created_at, reaction_id2);

        // Should have only one user reaction
        assert_eq!(chat_message.reactions.user_reactions.len(), 1);
        assert_eq!(chat_message.reactions.user_reactions[0].emoji, "❤");

        // Should have only the new emoji
        assert_eq!(chat_message.reactions.by_emoji.len(), 1);
        assert!(chat_message.reactions.by_emoji.contains_key("❤"));
        assert!(!chat_message.reactions.by_emoji.contains_key("👍"));
    }

    #[test]
    fn test_multiple_users_same_emoji() {
        let mut chat_message = create_chat_message("msg1");
        let user1 = Keys::generate().public_key();
        let user2 = Keys::generate().public_key();
        let created_at = Timestamp::from(1234567890);
        let reaction_id1 = EventId::all_zeros();
        let reaction_id2 =
            EventId::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();

        add_reaction_to_message(&mut chat_message, &user1, "👍", created_at, reaction_id1);
        add_reaction_to_message(&mut chat_message, &user2, "👍", created_at, reaction_id2);

        // Should have two user reactions
        assert_eq!(chat_message.reactions.user_reactions.len(), 2);

        // Should aggregate to one emoji with count 2
        assert_eq!(chat_message.reactions.by_emoji.len(), 1);
        let emoji_reaction = chat_message.reactions.by_emoji.get("👍").unwrap();
        assert_eq!(emoji_reaction.count, 2);
        assert_eq!(emoji_reaction.users.len(), 2);
    }

    #[test]
    fn test_add_reaction_to_message_sorting() {
        let mut chat_message = create_chat_message("msg1");
        let user1 = Keys::generate().public_key();
        let user2 = Keys::generate().public_key();

        // Add reactions with different timestamps
        let early_time = Timestamp::from(1000);
        let later_time = Timestamp::from(2000);
        let reaction_id1 = EventId::all_zeros();
        let reaction_id2 =
            EventId::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();

        add_reaction_to_message(&mut chat_message, &user1, "👍", later_time, reaction_id1);
        add_reaction_to_message(&mut chat_message, &user2, "❤", early_time, reaction_id2);

        // Should be sorted by timestamp
        assert_eq!(chat_message.reactions.user_reactions.len(), 2);
        assert_eq!(
            chat_message.reactions.user_reactions[0].created_at,
            early_time
        );
        assert_eq!(
            chat_message.reactions.user_reactions[1].created_at,
            later_time
        );
    }

    #[test]
    fn test_reaction_removal_when_count_zero() {
        let mut chat_message = create_chat_message("msg1");
        let user = Keys::generate().public_key();
        let created_at = Timestamp::from(1234567890);
        let reaction_id1 = EventId::all_zeros();
        let reaction_id2 =
            EventId::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();

        // Add reaction
        add_reaction_to_message(&mut chat_message, &user, "👍", created_at, reaction_id1);
        assert_eq!(chat_message.reactions.by_emoji.len(), 1);

        // Replace with different reaction (should remove the old one completely)
        add_reaction_to_message(&mut chat_message, &user, "❤", created_at, reaction_id2);

        // The 👍 emoji should be completely removed since count reached 0
        assert!(!chat_message.reactions.by_emoji.contains_key("👍"));
        assert!(chat_message.reactions.by_emoji.contains_key("❤"));
        assert_eq!(chat_message.reactions.by_emoji.len(), 1);
    }

    #[test]
    fn test_process_reaction_stores_message_id_as_reaction_id() {
        let target_msg = create_chat_message("target_msg_id");
        let mut processed_messages = HashMap::new();
        processed_messages.insert("target_msg_id".to_string(), target_msg);

        let keys = Keys::generate();
        let reaction_event_id =
            EventId::from_hex("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890")
                .unwrap();

        let mut tags = Tags::new();
        tags.push(Tag::parse(vec!["e", "target_msg_id"]).unwrap());

        let inner_event = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from(1234567890),
            Kind::Reaction,
            tags.clone(),
            "👍",
        );

        let message = Message {
            id: reaction_event_id,
            pubkey: keys.public_key(),
            created_at: Timestamp::from(1234567890),
            processed_at: Timestamp::from(1234567890),
            kind: Kind::Reaction,
            tags,
            content: "👍".to_string(),
            mls_group_id: GroupId::from_slice(&[1; 32]),
            event: inner_event,
            wrapper_event_id: EventId::all_zeros(),
            epoch: None,
            state: MessageState::Processed,
        };

        let config = AggregatorConfig::default();
        process_reaction(&message, &mut processed_messages, &config).unwrap();

        let target = processed_messages.get("target_msg_id").unwrap();
        assert_eq!(target.reactions.user_reactions.len(), 1);
        assert_eq!(
            target.reactions.user_reactions[0].reaction_id,
            reaction_event_id
        );
    }
}

//! Scheduled task to delete expired disappearing messages.

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::aggregated_message::AggregatedMessage;
use crate::whitenoise::chat_list_streaming::ChatListUpdateTrigger;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::message_aggregator::ChatMessage;
use crate::whitenoise::message_streaming::{MessageUpdate, UpdateTrigger};
use crate::whitenoise::scheduled_tasks::Task;

/// Scheduled task that periodically deletes messages whose `expires_at`
/// timestamp has passed.
///
/// When a group has disappearing messages enabled, every incoming and
/// outgoing message is stored with an `expires_at` value derived from the
/// group's configured duration. This task scans for rows past their expiry,
/// deletes them, and emits chat-list updates so the UI refreshes.
///
/// Runs every 30 seconds. This is a local-only operation — no deletion
/// events are published to relays (the outer kind:445 events already carry
/// NIP-40 expiration tags for relay-side cleanup).
pub(crate) struct DisappearingMessageCleanup;

#[async_trait]
impl Task for DisappearingMessageCleanup {
    fn name(&self) -> &'static str {
        "disappearing_message_cleanup"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(30)
    }

    #[perf_instrument("scheduled::disappearing_message_cleanup")]
    async fn execute(&self, whitenoise: &'static Whitenoise) -> Result<(), WhitenoiseError> {
        let now_ms = Utc::now().timestamp_millis();

        // Collect expired message IDs before deletion so we can notify subscribers.
        let expired_messages =
            AggregatedMessage::find_expired(now_ms, &whitenoise.database).await?;

        let affected_groups =
            AggregatedMessage::delete_expired(now_ms, &whitenoise.database).await?;

        if affected_groups.is_empty() {
            return Ok(());
        }

        // Emit per-message updates so open conversation views can remove them.
        for (message_id, group_id) in &expired_messages {
            whitenoise.message_stream_manager.emit(
                group_id,
                MessageUpdate {
                    trigger: UpdateTrigger::MessageExpired,
                    message: ChatMessage::expired_placeholder(message_id),
                },
            );
        }

        // Emit chat-list updates for each affected group so the UI refreshes
        // the last-message preview.
        for group_id in &affected_groups {
            whitenoise
                .emit_chat_list_update_for_group(
                    group_id,
                    ChatListUpdateTrigger::LastMessageDeleted,
                )
                .await;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_has_correct_name() {
        let task = DisappearingMessageCleanup;
        assert_eq!(task.name(), "disappearing_message_cleanup");
    }

    #[test]
    fn task_has_thirty_second_interval() {
        let task = DisappearingMessageCleanup;
        assert_eq!(task.interval(), Duration::from_secs(30));
    }
}

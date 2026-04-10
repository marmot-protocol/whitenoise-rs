//! Scheduled task to delete expired disappearing messages.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use nostr_sdk::{EventId, PublicKey};

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts_groups::AccountGroup;
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

        // Collect expired message IDs before deletion so we can notify subscribers
        // and delete them from each member's MDK storage.
        let expired_messages =
            AggregatedMessage::find_expired(now_ms, &whitenoise.database).await?;

        if expired_messages.is_empty() {
            return Ok(());
        }

        // Resolve which accounts hold MDK storage for each affected group. A
        // group is typically joined by one local account, but the codebase
        // supports multiple accounts on the same device, so we union them.
        let mut group_members: HashMap<Vec<u8>, HashSet<PublicKey>> = HashMap::new();
        for (_, group_id) in &expired_messages {
            let key = group_id.as_slice().to_vec();
            if group_members.contains_key(&key) {
                continue;
            }
            let account_groups =
                AccountGroup::find_by_group(group_id, &whitenoise.database).await?;
            let members: HashSet<PublicKey> = account_groups
                .into_iter()
                .map(|ag| ag.account_pubkey)
                .collect();
            group_members.insert(key, members);
        }

        // Delete each expired message from every member account's MDK
        // storage. We do this before the aggregated-messages DELETE so that a
        // crash mid-cleanup leaves the DB consistent with MDK (the task will
        // retry next tick). Failures are logged but do not abort the pass —
        // one bad account shouldn't hold the whole cleanup hostage.
        for (message_id_hex, group_id) in &expired_messages {
            let event_id = match EventId::from_hex(message_id_hex) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::disappearing_messages",
                        "Skipping expired message with unparseable id {}: {}",
                        message_id_hex,
                        e,
                    );
                    continue;
                }
            };

            let Some(members) = group_members.get(group_id.as_slice()) else {
                continue;
            };

            for account_pubkey in members {
                let mdk = match whitenoise.create_mdk_for_account(*account_pubkey) {
                    Ok(mdk) => mdk,
                    Err(e) => {
                        tracing::warn!(
                            target: "whitenoise::disappearing_messages",
                            "Failed to open MDK for account {} while cleaning group: {}",
                            account_pubkey.to_hex(),
                            e,
                        );
                        continue;
                    }
                };

                if let Err(e) = mdk.delete_message(group_id, &event_id) {
                    tracing::warn!(
                        target: "whitenoise::disappearing_messages",
                        "MDK delete_message failed for {} in group: {}",
                        message_id_hex,
                        e,
                    );
                }
            }
        }

        let affected_groups =
            AggregatedMessage::delete_expired(now_ms, &whitenoise.database).await?;

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

//! Scheduled task to clear expired chat mutes.

use std::time::Duration;

use async_trait::async_trait;

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::accounts_groups::AccountGroup;
use crate::whitenoise::chat_list_streaming::ChatListUpdateTrigger;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::scheduled_tasks::Task;

/// Scheduled task that periodically clears expired `muted_until` values
/// and emits `ChatMuteChanged` so both Flutter and the CLI react in real time.
///
/// Runs every 1 minute. Mute expiry is also checked at read time via
/// `is_muted()`, so this task is a convenience — it ensures the UI updates
/// promptly rather than waiting for the next user interaction.
pub(crate) struct MuteExpiryCleanup;

#[async_trait]
impl Task for MuteExpiryCleanup {
    fn name(&self) -> &'static str {
        "mute_expiry_cleanup"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(60) // 1 minute
    }

    #[perf_instrument("scheduled::mute_expiry_cleanup")]
    async fn execute(&self, whitenoise: &'static Whitenoise) -> Result<(), WhitenoiseError> {
        let cleared = AccountGroup::clear_expired_mutes(&whitenoise.database).await?;

        if cleared.is_empty() {
            tracing::debug!(
                target: "whitenoise::scheduler::mute_expiry_cleanup",
                "No expired mutes to clear"
            );
            return Ok(());
        }

        tracing::info!(
            target: "whitenoise::scheduler::mute_expiry_cleanup",
            "Cleared {} expired mute(s)",
            cleared.len()
        );

        // Emit ChatMuteChanged for each cleared mute so the UI updates.
        for ag in &cleared {
            match Account::find_by_pubkey(&ag.account_pubkey, &whitenoise.database).await {
                Ok(account) => {
                    whitenoise
                        .emit_chat_list_update(
                            &account,
                            &ag.mls_group_id,
                            ChatListUpdateTrigger::ChatMuteChanged,
                        )
                        .await;
                }
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::scheduler::mute_expiry_cleanup",
                        "Failed to find account {} for mute expiry update: {}",
                        ag.account_pubkey, e
                    );
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_has_correct_name() {
        let task = MuteExpiryCleanup;
        assert_eq!(task.name(), "mute_expiry_cleanup");
    }

    #[test]
    fn task_has_one_minute_interval() {
        let task = MuteExpiryCleanup;
        assert_eq!(task.interval(), Duration::from_secs(60));
    }
}

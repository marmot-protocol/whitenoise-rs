//! Scheduled task to clean up stale cached graph user entries.

use std::time::Duration;

use async_trait::async_trait;

use crate::whitenoise::Whitenoise;
use crate::whitenoise::cached_graph_user::CachedGraphUser;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::scheduled_tasks::Task;

/// Scheduled task that periodically removes stale cached graph user entries.
///
/// This task runs every 6 hours and deletes entries older than the default TTL (24 hours).
/// It helps keep the `cached_graph_users` table from growing unbounded during heavy search usage.
pub(crate) struct CachedGraphUserCleanup;

#[async_trait]
impl Task for CachedGraphUserCleanup {
    fn name(&self) -> &'static str {
        "cached_graph_user_cleanup"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(6 * 60 * 60) // 6 hours
    }

    async fn execute(&self, whitenoise: &'static Whitenoise) -> Result<(), WhitenoiseError> {
        let deleted = CachedGraphUser::cleanup_stale(&whitenoise.database).await?;

        if deleted > 0 {
            tracing::info!(
                target: "whitenoise::scheduler::cached_graph_user_cleanup",
                "Cleaned up {} stale cached graph user entries",
                deleted
            );
        } else {
            tracing::debug!(
                target: "whitenoise::scheduler::cached_graph_user_cleanup",
                "No stale cached graph user entries to clean up"
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_task_has_correct_name() {
        let task = CachedGraphUserCleanup;
        assert_eq!(task.name(), "cached_graph_user_cleanup");
    }

    #[test]
    fn cleanup_task_has_six_hour_interval() {
        let task = CachedGraphUserCleanup;
        assert_eq!(task.interval(), Duration::from_secs(6 * 60 * 60));
    }

    // Note: The actual cleanup logic is tested in database/cached_graph_users.rs
    // via CachedGraphUser::cleanup_stale tests. The Task::execute method is a
    // thin wrapper that calls cleanup_stale, so we only test the trait metadata here.
}

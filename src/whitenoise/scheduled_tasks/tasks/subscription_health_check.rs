//! Scheduled task to verify and recover relay subscriptions.

use std::time::Duration;

use async_trait::async_trait;

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::scheduled_tasks::Task;

/// Periodically checks that all relay subscriptions (discovery + per-account)
/// are operational and triggers a rebuild for any that have drifted.
///
/// This is the safety net for transient failures: if discovery sync fails at
/// startup or a relay disconnects mid-session, this task detects the gap and
/// recovers within one interval.
pub(crate) struct SubscriptionHealthCheck;

#[async_trait]
impl Task for SubscriptionHealthCheck {
    fn name(&self) -> &'static str {
        "subscription_health_check"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(15 * 60) // 15 minutes
    }

    #[perf_instrument("scheduled::subscription_health_check")]
    async fn execute(&self, whitenoise: &'static Whitenoise) -> Result<(), WhitenoiseError> {
        whitenoise.ensure_all_subscriptions().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_has_correct_name() {
        let task = SubscriptionHealthCheck;
        assert_eq!(task.name(), "subscription_health_check");
    }

    #[test]
    fn task_has_fifteen_minute_interval() {
        let task = SubscriptionHealthCheck;
        assert_eq!(task.interval(), Duration::from_secs(15 * 60));
    }
}

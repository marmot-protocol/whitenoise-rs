use async_trait::async_trait;

use super::worker::PreparedProductAnalyticsEvent;
use crate::whitenoise::Result;

#[async_trait]
pub(crate) trait ProductAnalyticsClient: Send + Sync {
    async fn send_events(&self, events: &[PreparedProductAnalyticsEvent]) -> Result<()>;
}

use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::broadcast;
use tokio::time::timeout;

use crate::whitenoise::error::Result;
use crate::whitenoise::notification_streaming::NotificationUpdate;
use crate::whitenoise::{Whitenoise, WhitenoiseConfig};

/// Status of the background notification collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundNotificationStatus {
    /// New notification data was collected.
    NewData,
    /// No new notifications were found.
    NoData,
    /// An error occurred during collection.
    Failed,
}

/// Result of a background notification collection pass.
#[derive(Debug, Clone, Serialize)]
pub struct BackgroundNotificationResult {
    pub status: BackgroundNotificationStatus,
    pub notifications: Vec<NotificationUpdate>,
    /// If status is `Failed`, contains the error description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl BackgroundNotificationResult {
    fn new_data(notifications: Vec<NotificationUpdate>) -> Self {
        Self {
            status: BackgroundNotificationStatus::NewData,
            notifications,
            error: None,
        }
    }

    fn no_data() -> Self {
        Self {
            status: BackgroundNotificationStatus::NoData,
            notifications: Vec::new(),
            error: None,
        }
    }

    fn failed(error: String) -> Self {
        Self {
            status: BackgroundNotificationStatus::Failed,
            notifications: Vec::new(),
            error: Some(error),
        }
    }
}

/// Default quiet window: if no notification arrives within this duration, stop collecting.
const DEFAULT_QUIET_WINDOW: Duration = Duration::from_millis(1000);

/// Collect notification updates after a background push wake.
///
/// This is the core async function that:
/// 1. Ensures Whitenoise is initialized (cold or warm start).
/// 2. Subscribes to the notification broadcast channel.
/// 3. Refreshes relay subscriptions to fetch missed events.
/// 4. Collects emitted `NotificationUpdate`s within a bounded time window.
///
/// The function uses a quiet-window strategy: it stops collecting once no new
/// notification arrives for 1 second (the quiet window), or when `max_wait` is
/// reached, whichever comes first.
///
/// # Arguments
///
/// * `config` - Whitenoise configuration (data dir, logs dir, keyring service).
///   Ignored if Whitenoise is already initialized.
/// * `max_wait` - Hard deadline for the entire collection pass. iOS background
///   execution is limited to ~30 seconds; this should be well under that.
pub async fn collect_notifications_after_push(
    config: WhitenoiseConfig,
    max_wait: Duration,
) -> BackgroundNotificationResult {
    match collect_notifications_inner(config, max_wait).await {
        Ok(result) => result,
        Err(e) => {
            tracing::error!(
                target: "whitenoise::background_notifications",
                "Background notification collection failed: {}",
                e
            );
            BackgroundNotificationResult::failed(e.to_string())
        }
    }
}

async fn collect_notifications_inner(
    config: WhitenoiseConfig,
    max_wait: Duration,
) -> Result<BackgroundNotificationResult> {
    tracing::info!(
        target: "whitenoise::background_notifications",
        max_wait_ms = max_wait.as_millis() as u64,
        "Starting background notification collection"
    );

    // Step 1: Ensure Whitenoise is initialized (no-op if warm, full boot if cold).
    let whitenoise = Whitenoise::ensure_initialized(config).await?;

    // Step 2: Subscribe to notification broadcast BEFORE refreshing subscriptions.
    // This ordering is critical — events processed after ensure_all_subscriptions()
    // will be emitted to the broadcast, and our subscriber must be in place to
    // receive them. The `has_subscribers()` gate in the emit methods would otherwise
    // drop them silently.
    let mut rx = whitenoise.subscribe_to_notifications().updates;

    // Step 3: Refresh relay subscriptions. This reconnects dead relays and
    // fetches missed events using `since` timestamps. The event processor
    // decrypts and processes them, emitting NotificationUpdates.
    if let Err(e) = whitenoise.ensure_all_subscriptions().await {
        tracing::warn!(
            target: "whitenoise::background_notifications",
            "ensure_all_subscriptions failed (continuing with collection): {}",
            e
        );
    }

    tracing::debug!(
        target: "whitenoise::background_notifications",
        "Subscriptions refreshed, starting collection window"
    );

    // Step 4: Collect notifications with quiet-window + hard deadline.
    let deadline = Instant::now() + max_wait;
    let mut collected = Vec::new();

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            tracing::debug!(
                target: "whitenoise::background_notifications",
                "Hard deadline reached after collecting {} notifications",
                collected.len()
            );
            break;
        }

        let wait = remaining.min(DEFAULT_QUIET_WINDOW);
        match timeout(wait, rx.recv()).await {
            Ok(Ok(update)) => {
                tracing::debug!(
                    target: "whitenoise::background_notifications",
                    trigger = ?update.trigger,
                    is_dm = update.is_dm,
                    group_name = ?update.group_name,
                    "Collected notification"
                );
                collected.push(update);
            }
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                tracing::warn!(
                    target: "whitenoise::background_notifications",
                    "Notification receiver lagged by {} messages",
                    n
                );
                continue;
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                tracing::debug!(
                    target: "whitenoise::background_notifications",
                    "Notification channel closed"
                );
                break;
            }
            Err(_) => {
                // Timeout — quiet window expired with no new notification.
                tracing::debug!(
                    target: "whitenoise::background_notifications",
                    "Quiet window expired after collecting {} notifications",
                    collected.len()
                );
                break;
            }
        }
    }

    tracing::info!(
        target: "whitenoise::background_notifications",
        count = collected.len(),
        "Background notification collection complete"
    );

    if collected.is_empty() {
        Ok(BackgroundNotificationResult::no_data())
    } else {
        Ok(BackgroundNotificationResult::new_data(collected))
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use mdk_core::prelude::GroupId;
    use nostr_sdk::Keys;

    use super::*;
    use crate::whitenoise::notification_streaming::{
        NotificationTrigger, NotificationUpdate, NotificationUser,
    };

    fn make_test_notification(trigger: NotificationTrigger, content: &str) -> NotificationUpdate {
        let keys = Keys::generate();
        let sender_keys = Keys::generate();

        NotificationUpdate {
            trigger,
            mls_group_id: GroupId::from_slice(&[1, 2, 3, 4]),
            group_name: Some("Test Group".to_string()),
            is_dm: false,
            receiver: NotificationUser {
                pubkey: keys.public_key(),
                display_name: Some("Alice".to_string()),
                picture_url: None,
            },
            sender: NotificationUser {
                pubkey: sender_keys.public_key(),
                display_name: Some("Bob".to_string()),
                picture_url: None,
            },
            content: content.to_string(),
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn result_new_data_serializes_correctly() {
        let notification = make_test_notification(NotificationTrigger::NewMessage, "Hello world");
        let result = BackgroundNotificationResult::new_data(vec![notification]);

        let json = serde_json::to_string_pretty(&result).expect("serialization failed");

        assert!(json.contains("\"status\": \"new_data\""));
        assert!(json.contains("\"trigger\": \"NewMessage\""));
        assert!(json.contains("Hello world"));
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn result_no_data_serializes_correctly() {
        let result = BackgroundNotificationResult::no_data();

        let json = serde_json::to_string_pretty(&result).expect("serialization failed");

        assert!(json.contains("\"status\": \"no_data\""));
        assert!(json.contains("\"notifications\": []"));
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn result_failed_serializes_correctly() {
        let result = BackgroundNotificationResult::failed("something broke".to_string());

        let json = serde_json::to_string_pretty(&result).expect("serialization failed");

        assert!(json.contains("\"status\": \"failed\""));
        assert!(json.contains("\"error\": \"something broke\""));
        assert!(json.contains("\"notifications\": []"));
    }

    #[test]
    fn result_multiple_notifications_serializes_correctly() {
        let n1 = make_test_notification(NotificationTrigger::NewMessage, "msg 1");
        let n2 = make_test_notification(NotificationTrigger::GroupInvite, "");
        let n3 = make_test_notification(NotificationTrigger::NewMessage, "msg 2");
        let result = BackgroundNotificationResult::new_data(vec![n1, n2, n3]);

        let json = serde_json::to_string(&result).expect("serialization failed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("deserialization failed");

        assert_eq!(parsed["notifications"].as_array().unwrap().len(), 3);
        assert_eq!(parsed["notifications"][1]["trigger"], "GroupInvite");
    }
}

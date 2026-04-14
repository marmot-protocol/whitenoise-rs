use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::broadcast;
use tokio::time::timeout;

use crate::whitenoise::error::Result;
use crate::whitenoise::notification_streaming::{NotificationTrigger, NotificationUpdate};
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

/// A notification user in the DTO payload (pubkey as hex string).
#[derive(Debug, Clone, Serialize)]
pub struct NotificationUserDto {
    /// Hex-encoded public key.
    pub pubkey: String,
    pub display_name: Option<String>,
    pub picture_url: Option<String>,
}

/// A single notification entry in the JSON payload returned to iOS.
///
/// This DTO is the stable external contract for the iOS background push
/// integration. It is intentionally decoupled from `NotificationUpdate` so
/// the internal broadcast type can evolve without breaking the Swift side.
#[derive(Debug, Clone, Serialize)]
pub struct NotificationDto {
    /// Trigger as a lowercase string: `"message"` or `"invite"`.
    pub trigger: &'static str,
    /// Hex-encoded MLS group id.
    pub mls_group_id: String,
    pub group_name: Option<String>,
    pub is_dm: bool,
    pub receiver: NotificationUserDto,
    pub sender: NotificationUserDto,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

impl NotificationDto {
    fn from_update(update: NotificationUpdate) -> Self {
        Self {
            trigger: trigger_value(update.trigger),
            mls_group_id: hex::encode(update.mls_group_id.as_slice()),
            group_name: update.group_name,
            is_dm: update.is_dm,
            receiver: NotificationUserDto {
                pubkey: update.receiver.pubkey.to_hex(),
                display_name: update.receiver.display_name,
                picture_url: update.receiver.picture_url,
            },
            sender: NotificationUserDto {
                pubkey: update.sender.pubkey.to_hex(),
                display_name: update.sender.display_name,
                picture_url: update.sender.picture_url,
            },
            content: update.content,
            timestamp: update.timestamp,
        }
    }
}

fn trigger_value(trigger: NotificationTrigger) -> &'static str {
    match trigger {
        NotificationTrigger::NewMessage => "message",
        NotificationTrigger::GroupInvite => "invite",
    }
}

/// Result of a background notification collection pass.
#[derive(Debug, Clone, Serialize)]
pub struct BackgroundNotificationResult {
    pub status: BackgroundNotificationStatus,
    pub notifications: Vec<NotificationDto>,
    /// If status is `Failed`, contains the error description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl BackgroundNotificationResult {
    fn new_data(notifications: Vec<NotificationUpdate>) -> Self {
        Self {
            status: BackgroundNotificationStatus::NewData,
            notifications: notifications
                .into_iter()
                .map(NotificationDto::from_update)
                .collect(),
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

    /// Public so the FFI layer can construct failure results without
    /// duplicating the JSON shape.
    pub fn failed(error: String) -> Self {
        Self {
            status: BackgroundNotificationStatus::Failed,
            notifications: Vec::new(),
            error: Some(error),
        }
    }
}

/// Default quiet window: if no notification arrives within this duration, stop collecting.
const DEFAULT_QUIET_WINDOW: Duration = Duration::from_millis(1000);

/// Hard ceiling on any caller-provided `max_wait`. iOS gives silent-push
/// handlers roughly 30 seconds of background execution; we clamp to 25s to
/// leave a safety margin for init, response serialization, and the native
/// local-notification scheduling that happens after this returns.
const IOS_MAX_WAIT: Duration = Duration::from_secs(25);

/// Upper bound on the number of notifications returned in a single collection
/// pass. Guards against memory growth during relay bursts and matches the UX
/// reality that a user cannot meaningfully triage dozens of notifications from
/// a single background wake.
const MAX_COLLECTED: usize = 50;

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
    // Clamp caller-provided wait to an iOS-safe ceiling. Protects against
    // accidental long waits from buggy callers that might otherwise outlive
    // the iOS background execution budget and get the process killed.
    let max_wait = max_wait.min(IOS_MAX_WAIT);
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
    // Propagate errors here — if subscriptions fail to refresh, no events
    // will arrive and the caller should receive an explicit failure rather
    // than a misleading "no_data" result.
    whitenoise.ensure_all_subscriptions().await?;

    tracing::debug!(
        target: "whitenoise::background_notifications",
        "Subscriptions refreshed, starting collection window"
    );

    // Step 4: Collect notifications with quiet-window + hard deadline +
    // size cap (MAX_COLLECTED).
    let collected = drain_notifications(&mut rx, max_wait).await;

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

/// Drain notification updates from a receiver until one of the following stops:
/// * the quiet window expires with no new notification,
/// * the hard deadline (`max_wait`) is reached,
/// * `MAX_COLLECTED` notifications have been collected,
/// * the channel closes.
///
/// Extracted so it can be unit-tested against a synthetic
/// `broadcast::Receiver` without needing a full `Whitenoise` instance.
async fn drain_notifications(
    rx: &mut broadcast::Receiver<NotificationUpdate>,
    max_wait: Duration,
) -> Vec<NotificationUpdate> {
    let deadline = Instant::now() + max_wait;
    let mut collected = Vec::with_capacity(MAX_COLLECTED);

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
                if collected.len() >= MAX_COLLECTED {
                    tracing::warn!(
                        target: "whitenoise::background_notifications",
                        cap = MAX_COLLECTED,
                        "Notification collection cap reached; stopping early"
                    );
                    break;
                }
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

    collected
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
            mls_group_id: GroupId::from_slice(&[0xab, 0xcd, 0xef, 0x01]),
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

    fn parse(result: &BackgroundNotificationResult) -> serde_json::Value {
        let json = serde_json::to_string(result).expect("serialization failed");
        serde_json::from_str(&json).expect("deserialization failed")
    }

    #[test]
    fn result_new_data_shape() {
        let notification = make_test_notification(NotificationTrigger::NewMessage, "Hello world");
        let result = BackgroundNotificationResult::new_data(vec![notification]);
        let v = parse(&result);

        assert_eq!(v["status"], "new_data");
        assert!(v.get("error").is_none());
        let notifications = v["notifications"].as_array().expect("notifications array");
        assert_eq!(notifications.len(), 1);
        let n = &notifications[0];
        assert_eq!(n["trigger"], "message");
        assert_eq!(n["mls_group_id"], "abcdef01");
        assert_eq!(n["group_name"], "Test Group");
        assert_eq!(n["is_dm"], false);
        assert_eq!(n["content"], "Hello world");
        // pubkey is serialized as a hex string (64 chars for 32-byte key).
        let sender_pubkey = n["sender"]["pubkey"].as_str().expect("sender pubkey");
        assert_eq!(sender_pubkey.len(), 64);
        assert!(sender_pubkey.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(n["sender"]["display_name"], "Bob");
        assert!(n["sender"]["picture_url"].is_null());
    }

    #[test]
    fn result_invite_trigger_is_invite() {
        let notification = make_test_notification(NotificationTrigger::GroupInvite, "");
        let result = BackgroundNotificationResult::new_data(vec![notification]);
        let v = parse(&result);
        assert_eq!(v["notifications"][0]["trigger"], "invite");
    }

    #[test]
    fn result_no_data_shape() {
        let result = BackgroundNotificationResult::no_data();
        let v = parse(&result);
        assert_eq!(v["status"], "no_data");
        assert_eq!(v["notifications"].as_array().unwrap().len(), 0);
        assert!(v.get("error").is_none());
    }

    #[test]
    fn result_failed_shape() {
        let result = BackgroundNotificationResult::failed("something broke".to_string());
        let v = parse(&result);
        assert_eq!(v["status"], "failed");
        assert_eq!(v["error"], "something broke");
        assert_eq!(v["notifications"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn result_multiple_notifications() {
        let n1 = make_test_notification(NotificationTrigger::NewMessage, "msg 1");
        let n2 = make_test_notification(NotificationTrigger::GroupInvite, "");
        let n3 = make_test_notification(NotificationTrigger::NewMessage, "msg 2");
        let result = BackgroundNotificationResult::new_data(vec![n1, n2, n3]);
        let v = parse(&result);

        let notifications = v["notifications"].as_array().unwrap();
        assert_eq!(notifications.len(), 3);
        assert_eq!(notifications[0]["trigger"], "message");
        assert_eq!(notifications[1]["trigger"], "invite");
        assert_eq!(notifications[2]["trigger"], "message");
    }

    #[tokio::test(start_paused = true)]
    async fn drain_notifications_caps_at_max_collected() {
        // Create a standalone broadcast channel and pre-fill it with more
        // than MAX_COLLECTED notifications before starting the drain. The
        // drain must stop at exactly MAX_COLLECTED even though more items
        // are waiting in the channel and the deadline has not been reached.
        let flood = MAX_COLLECTED + 25;
        let (tx, mut rx) = broadcast::channel(flood);
        for i in 0..flood {
            let content = format!("msg {i}");
            tx.send(make_test_notification(
                NotificationTrigger::NewMessage,
                &content,
            ))
            .expect("send should succeed while rx is alive");
        }

        // Generous deadline — we want the cap, not the deadline, to stop us.
        let collected = drain_notifications(&mut rx, Duration::from_secs(60)).await;

        assert_eq!(
            collected.len(),
            MAX_COLLECTED,
            "drain should stop exactly at MAX_COLLECTED"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn drain_notifications_quiet_window_stops_below_cap() {
        // If fewer than MAX_COLLECTED notifications arrive and the channel
        // goes quiet, the quiet window should trip instead of the cap.
        let (tx, mut rx) = broadcast::channel(16);
        for i in 0..5 {
            let content = format!("msg {i}");
            tx.send(make_test_notification(
                NotificationTrigger::NewMessage,
                &content,
            ))
            .expect("send should succeed while rx is alive");
        }

        let collected = drain_notifications(&mut rx, Duration::from_secs(60)).await;
        assert_eq!(collected.len(), 5);
    }

    #[test]
    fn ios_max_wait_is_under_ios_budget() {
        // Sanity check: the clamp must be strictly below the iOS 30s ceiling
        // so we never outlive the system-provided background budget.
        assert!(IOS_MAX_WAIT < Duration::from_secs(30));
    }
}

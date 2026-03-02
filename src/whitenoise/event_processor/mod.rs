use nostr_sdk::prelude::*;
use tokio::sync::mpsc::Receiver;

use crate::{
    nostr_manager::utils::is_event_timestamp_valid,
    types::{ProcessableEvent, RetryInfo},
    whitenoise::{
        Whitenoise,
        error::{Result, WhitenoiseError},
    },
};

mod account_event_processor;
mod event_handlers;
mod global_event_processor;

impl Whitenoise {
    /// Start the event processing loop in a background task
    pub(crate) async fn start_event_processing_loop(
        whitenoise: &'static Whitenoise,
        receiver: Receiver<ProcessableEvent>,
        shutdown_receiver: Receiver<()>,
    ) {
        tokio::spawn(async move {
            Self::process_events(whitenoise, receiver, shutdown_receiver).await;
        });
    }

    /// Shutdown event processing gracefully
    pub(crate) async fn shutdown_event_processing(&self) -> Result<()> {
        match self.shutdown_sender.send(()).await {
            Ok(_) => Ok(()),
            Err(_) => Ok(()), // Expected if processor already shut down
        }
    }

    /// Main event processing loop
    async fn process_events(
        whitenoise: &'static Whitenoise,
        mut receiver: Receiver<ProcessableEvent>,
        mut shutdown: Receiver<()>,
    ) {
        tracing::debug!(
            target: "whitenoise::event_processor::process_events",
            "Starting event processing loop"
        );

        let mut shutting_down = false;

        loop {
            tokio::select! {
                Some(event) = receiver.recv() => {
                    tracing::debug!(
                        target: "whitenoise::event_processor::process_events",
                        "Received event for processing"
                    );

                    // Process the event
                    match event {
                        ProcessableEvent::NostrEvent { event, subscription_id, retry_info } => {
                            // Validate timestamp before processing
                            if !is_event_timestamp_valid(&event) {
                                tracing::debug!(
                                    target: "whitenoise::event_processor::process_events",
                                    "Skipping event {} with invalid future timestamp: {}",
                                    event.id.to_hex(),
                                    event.created_at
                                );
                                continue;
                            }

                            let sub_id = match &subscription_id {
                                Some(s) => s.clone(),
                                None => {
                                    tracing::warn!(
                                        target: "whitenoise::event_processor::process_events",
                                        "Event received without subscription ID, skipping"
                                    );
                                    continue;
                                }
                            };
                            if whitenoise.is_event_global(&sub_id) {
                                whitenoise.process_global_event(event, sub_id, retry_info).await;
                            } else {
                                whitenoise.process_account_event(event, sub_id, retry_info).await;
                            }
                        }
                        ProcessableEvent::RelayMessage(relay_url, message) => {
                            whitenoise.process_relay_message(relay_url, message).await;
                        }
                    }
                }
                Some(_) = shutdown.recv(), if !shutting_down => {
                    tracing::info!(
                        target: "whitenoise::event_processor::process_events",
                        "Received shutdown signal, finishing current queue..."
                    );
                    shutting_down = true;
                    // Continue processing remaining events in queue, but don't wait for new shutdown signals
                }
                else => {
                    if shutting_down {
                        tracing::debug!(
                            target: "whitenoise::event_processor::process_events",
                            "Queue flushed, shutting down event processor"
                        );
                    } else {
                        tracing::debug!(
                            target: "whitenoise::event_processor::process_events",
                            "All channels closed, exiting event processing loop"
                        );
                    }
                    break;
                }
            }
        }
    }

    /// Process relay messages for logging/monitoring
    async fn process_relay_message(&self, relay_url: RelayUrl, message_type: String) {
        tracing::debug!(
            target: "whitenoise::event_processor::process_relay_message",
            "Processing message from {}: {}",
            relay_url,
            message_type
        );
    }

    fn is_event_global(&self, subscription_id: &str) -> bool {
        subscription_id.starts_with("global_users_")
    }

    /// Schedule a retry for a failed event processing attempt
    fn schedule_retry(
        &self,
        event: Event,
        subscription_id: String,
        retry_info: RetryInfo,
        error: WhitenoiseError,
    ) {
        if let Some(next_retry) = retry_info.next_attempt() {
            let delay_ms = next_retry.delay_ms();
            tracing::warn!(
                target: "whitenoise::event_processor::schedule_retry",
                "Event processing failed (attempt {}/{}), retrying in {}ms: {}",
                next_retry.attempt,
                next_retry.max_attempts,
                delay_ms,
                error
            );

            let retry_event = ProcessableEvent::NostrEvent {
                event,
                subscription_id: Some(subscription_id),
                retry_info: next_retry,
            };
            let sender = self.event_sender.clone();

            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                if let Err(send_err) = sender.send(retry_event).await {
                    tracing::error!(
                        target: "whitenoise::event_processor::schedule_retry",
                        "Failed to requeue event for retry: {}",
                        send_err
                    );
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nostr_sdk::prelude::*;

    use crate::nostr_manager::utils::is_event_timestamp_valid;
    use crate::types::RetryInfo;
    use crate::whitenoise::error::WhitenoiseError;
    use crate::whitenoise::test_utils::*;

    #[tokio::test]
    async fn test_shutdown_event_processing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let result = whitenoise.shutdown_event_processing().await;
        assert!(result.is_ok());

        // Test that multiple shutdowns don't cause errors
        let result2 = whitenoise.shutdown_event_processing().await;
        assert!(result2.is_ok());
    }

    #[tokio::test]
    async fn test_queue_operations_after_shutdown() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        whitenoise.shutdown_event_processing().await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Test that shutdown completed successfully without errors
        // (We can't test queuing operations since those methods were removed)
    }

    #[tokio::test]
    async fn test_future_timestamp_rejection() {
        let keys = Keys::generate();

        // Create an event with a timestamp far in the future (exceeds MAX_FUTURE_SKEW of 1 hour)
        let far_future = Timestamp::now() + Duration::from_secs(7200); // 2 hours in future
        let event = EventBuilder::text_note("test message with future timestamp")
            .custom_created_at(far_future)
            .sign(&keys)
            .await
            .unwrap();

        // Validate that our utility function correctly identifies this as invalid
        assert!(!is_event_timestamp_valid(&event));

        // Test that a current timestamp is valid for comparison
        let valid_event = EventBuilder::text_note("test message with current timestamp")
            .sign(&keys)
            .await
            .unwrap();
        assert!(is_event_timestamp_valid(&valid_event));
    }

    #[tokio::test]
    async fn test_is_event_global() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;

        assert!(whitenoise.is_event_global("global_users_abc123_0"));
        assert!(whitenoise.is_event_global("global_users_xyz_1"));
        assert!(!whitenoise.is_event_global("account_abc123_events"));
        assert!(!whitenoise.is_event_global("abc123"));
        assert!(!whitenoise.is_event_global(""));
    }

    #[tokio::test]
    async fn test_process_relay_message() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;

        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();

        // Should complete without error (just logs)
        whitenoise
            .process_relay_message(relay_url, "EOSE".to_string())
            .await;
    }

    #[tokio::test]
    async fn test_schedule_retry_enqueues_event() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let event = EventBuilder::text_note("retry me")
            .sign(&keys)
            .await
            .unwrap();

        let retry_info = RetryInfo::new(); // attempt=0, max=10
        let error = WhitenoiseError::EventProcessor("test error".to_string());

        // Should not panic; spawns a delayed re-queue task
        whitenoise.schedule_retry(
            event,
            "global_users_abc123_0".to_string(),
            retry_info,
            error,
        );

        // Give the spawned task a moment (delay is 1s for attempt 0, so just verify no panic)
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_schedule_retry_exhausted_does_nothing() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let event = EventBuilder::text_note("no more retries")
            .sign(&keys)
            .await
            .unwrap();

        // Exhaust retry attempts
        let retry_info = RetryInfo {
            attempt: 10,
            max_attempts: 10,
            base_delay_ms: 1000,
        };
        let error = WhitenoiseError::EventProcessor("exhausted".to_string());

        // Should not spawn any task since retries are exhausted
        whitenoise.schedule_retry(
            event,
            "global_users_abc123_0".to_string(),
            retry_info,
            error,
        );
    }
}

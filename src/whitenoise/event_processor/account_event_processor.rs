use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};

use crate::{
    nostr_manager::utils::cap_timestamp_to_now,
    types::RetryInfo,
    whitenoise::{
        Whitenoise,
        accounts::Account,
        error::{Result, WhitenoiseError},
    },
};

impl Whitenoise {
    pub(super) async fn process_account_event(
        &self,
        event: Event,
        subscription_id: String,
        retry_info: RetryInfo,
    ) {
        // Get the account from the subscription ID, skip if we can't find it
        let account = match self
            .account_from_subscription_id(subscription_id.clone())
            .await
        {
            Ok(account) => account,
            Err(e) => {
                tracing::debug!(
                    target: "whitenoise::event_processor::process_account_event",
                    "Skipping event {}: Cannot find account for subscription ID: {}",
                    event.id.to_hex(),
                    e
                );
                return; // Skip - no retry
            }
        };

        // Check if we should skip this event (already processed or self-published)
        match self
            .should_skip_account_event_processing(&event, &account)
            .await
        {
            Ok(Some(skip_reason)) => {
                tracing::debug!(
                    target: "whitenoise::event_processor::process_account_event",
                    "Skipping event {}: {} (kind {})",
                    event.id.to_hex(),
                    skip_reason,
                    event.kind.as_u16()
                );
                return; // Skip - no retry
            }
            Ok(None) => {
                // Continue processing
            }
            Err(e) => {
                tracing::error!(
                    target: "whitenoise::event_processor::process_account_event",
                    "Skip check failed for event {}, continuing with processing: {}",
                    event.id.to_hex(),
                    e
                );
                // Continue processing despite skip check failure
            }
        }

        // Route the event to the appropriate handler
        tracing::info!(
            target: "whitenoise::event_processor::process_account_event",
            "Processing event {} (kind {}) for account {}",
            event.id.to_hex(),
            event.kind.as_u16(),
            account.pubkey.to_hex()
        );

        let result = self
            .route_account_event_for_processing(&event, &account)
            .await;

        // Handle the result - success, retry, or give up
        match result {
            Ok(()) => {
                // Record that we processed this event successfully
                if let Err(e) = self
                    .nostr
                    .event_tracker
                    .track_processed_account_event(&event, &account.pubkey)
                    .await
                {
                    tracing::warn!(
                        target: "whitenoise::event_processor::process_account_event",
                        "Failed to record processed event {}: {}",
                        event.id.to_hex(),
                        e
                    );
                }

                // Advance account sync timestamp only for account-scoped events
                match event.kind {
                    Kind::ContactList | Kind::MlsGroupMessage => {
                        // Cap timestamp to prevent future corruption
                        let safe_timestamp = cap_timestamp_to_now(event.created_at);
                        let created_ms = (safe_timestamp.as_u64() as i64) * 1000;

                        if let Err(e) = Account::update_last_synced_max(
                            &account.pubkey,
                            created_ms,
                            &self.database,
                        )
                        .await
                        {
                            tracing::warn!(
                                target: "whitenoise::event_processor::process_account_event",
                                "Failed to advance last_synced_at for {} on event {}: {}",
                                account.pubkey.to_hex(),
                                event.id.to_hex(),
                                e
                            );
                        } else {
                            tracing::debug!(
                                target: "whitenoise::event_processor::process_account_event",
                                "Successfully updated last_synced_at for {} with {} ms (event {})",
                                account.pubkey.to_hex(),
                                created_ms,
                                event.id.to_hex()
                            );
                        }
                    }
                    Kind::GiftWrap => {
                        // Use rumor timestamp for advancement per NIP-59
                        match self
                            .extract_rumor_timestamp_for_advancement(&event, &account)
                            .await
                        {
                            Ok(Some(rumor_timestamp)) => {
                                let safe_timestamp = cap_timestamp_to_now(rumor_timestamp);
                                let created_ms = (safe_timestamp.as_u64() as i64) * 1000;

                                if let Err(e) = Account::update_last_synced_max(
                                    &account.pubkey,
                                    created_ms,
                                    &self.database,
                                )
                                .await
                                {
                                    tracing::warn!(
                                        target: "whitenoise::event_processor::process_account_event",
                                        "Failed to advance last_synced_at with rumor timestamp for {}: {}",
                                        account.pubkey.to_hex(),
                                        e
                                    );
                                } else {
                                    tracing::debug!(
                                        target: "whitenoise::event_processor::process_account_event",
                                        "Updated last_synced_at (if older) for {} with rumor timestamp {} (ms) [capped from original {}]",
                                        account.pubkey.to_hex(),
                                        created_ms,
                                        rumor_timestamp.as_u64() * 1000
                                    );
                                }
                            }
                            Ok(None) => {
                                tracing::debug!(
                                    target: "whitenoise::event_processor::process_account_event",
                                    "Skipping timestamp advancement for giftwrap with failed rumor extraction"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    target: "whitenoise::event_processor::process_account_event",
                                    "Failed to extract rumor timestamp for {}: {}",
                                    account.pubkey.to_hex(),
                                    e
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
            Err(e) => {
                // Handle retry logic for actual processing errors
                if retry_info.should_retry() {
                    self.schedule_retry(event, subscription_id, retry_info, e);
                } else {
                    tracing::error!(
                        target: "whitenoise::event_processor::process_account_event",
                        "Event processing failed after {} attempts, giving up: {}",
                        retry_info.max_attempts,
                        e
                    );
                }
            }
        }
    }

    /// Extract the account pubkey from a subscription_id
    /// Subscription IDs follow the format: {hashed_pubkey}_{subscription_type}
    /// where hashed_pubkey = SHA256(session salt || accouny_pubkey)[..12]
    async fn extract_pubkey_from_subscription_id(
        &self,
        subscription_id: &str,
    ) -> Result<PublicKey> {
        let underscore_pos = subscription_id.find('_');
        if underscore_pos.is_none() {
            return Err(WhitenoiseError::InvalidEvent(format!(
                "Invalid subscription ID: {}",
                subscription_id
            )));
        }

        let hash_str = &subscription_id[..underscore_pos.unwrap()];
        // Get all accounts and find the one whose hash matches
        let accounts = Account::all(&self.database).await?;
        for account in accounts.iter() {
            let mut hasher = Sha256::new();
            hasher.update(self.nostr.session_salt());
            hasher.update(account.pubkey.to_bytes());
            let hash = hasher.finalize();
            let pubkey_hash = format!("{:x}", hash)[..12].to_string();
            if pubkey_hash == hash_str {
                return Ok(account.pubkey);
            }
        }

        Err(WhitenoiseError::InvalidEvent(format!(
            "No account found for subscription hash: {}",
            hash_str
        )))
    }

    async fn account_from_subscription_id(&self, subscription_id: String) -> Result<Account> {
        let target_pubkey = self
            .extract_pubkey_from_subscription_id(&subscription_id)
            .await
            .map_err(|_| {
                WhitenoiseError::InvalidEvent(format!(
                    "Cannot extract pubkey from subscription ID: {}",
                    subscription_id
                ))
            })?;

        tracing::debug!(
        target: "whitenoise::event_processor::process_mls_message",
        "Processing MLS message for account: {}",
        target_pubkey.to_hex()
        );

        Account::find_by_pubkey(&target_pubkey, &self.database).await
    }

    /// Check if an account event should be skipped (not processed)
    /// Returns Some(reason) if should skip, None if should process
    async fn should_skip_account_event_processing(
        &self,
        event: &Event,
        account: &Account,
    ) -> Result<Option<&'static str>> {
        let already_processed = match self
            .nostr
            .event_tracker
            .already_processed_account_event(&event.id, &account.pubkey)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(
                    target: "whitenoise::event_processor::should_skip_account_event_processing",
                    "Already processed check failed for {}: {}",
                    event.id.to_hex(),
                    e
                );
                false
            }
        };

        if already_processed {
            return Ok(Some("already processed"));
        }

        // For account-specific events, check if WE published this event
        // We don't skip giftwraps and MLS messages because we need them to process in nostr-mls
        let should_skip = match event.kind {
            Kind::MlsGroupMessage => false,
            Kind::GiftWrap => false,
            _ => match self
                .nostr
                .event_tracker
                .account_published_event(&event.id, &account.pubkey)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(
                        target: "whitenoise::event_processor::should_skip_account_event_processing",
                        "Account published check failed for {}: {}",
                        event.id.to_hex(),
                        e
                    );
                    false
                }
            },
        };

        if should_skip {
            return Ok(Some("self-published event"));
        }

        Ok(None) // Should process
    }

    /// Route an event to the appropriate handler based on its kind
    async fn route_account_event_for_processing(
        &self,
        event: &Event,
        account: &Account,
    ) -> Result<()> {
        match event.kind {
            Kind::GiftWrap => match validate_giftwrap_target(account, event) {
                Ok(()) => self.handle_giftwrap(account, event.clone()).await,
                Err(e) => Err(e),
            },
            Kind::MlsGroupMessage => self.handle_mls_message(account, event.clone()).await,
            Kind::Metadata => self.handle_metadata(event.clone()).await,
            Kind::RelayList | Kind::InboxRelays | Kind::MlsKeyPackageRelays => {
                self.handle_relay_list(event.clone()).await
            }
            Kind::ContactList => self.handle_contact_list(account, event.clone()).await,
            _ => {
                tracing::debug!(
                    target: "whitenoise::event_processor::route_event_for_processing",
                    "Received unhandled account event of kind: {:?} - add handler if needed",
                    event.kind
                );
                Ok(()) // Unhandled events are not errors
            }
        }
    }

    /// Extract rumor timestamp from giftwrap event for sync advancement
    async fn extract_rumor_timestamp_for_advancement(
        &self,
        event: &Event,
        account: &Account,
    ) -> Result<Option<Timestamp>> {
        let signer = self.get_signer_for_account(account)?;

        match extract_rumor(&signer, event).await {
            Ok(unwrapped) => Ok(Some(unwrapped.rumor.created_at)),
            Err(_) => Ok(None), // Don't advance on extraction failure
        }
    }
}

fn validate_giftwrap_target(account: &Account, event: &Event) -> Result<()> {
    // Extract the target pubkey from the event's 'p' tag
    let target_pubkey = event
        .tags
        .iter()
        .find(|tag| tag.kind() == TagKind::p())
        .and_then(|tag| tag.content())
        .and_then(|pubkey_str| PublicKey::parse(pubkey_str).ok())
        .ok_or_else(|| {
            WhitenoiseError::InvalidEvent(
                "No valid target pubkey found in 'p' tag for giftwrap event".to_string(),
            )
        })?;

    if target_pubkey != account.pubkey {
        return Err(WhitenoiseError::InvalidEvent(format!(
            "Giftwrap target pubkey {} does not match account pubkey {} - possible routing error",
            target_pubkey.to_hex(),
            account.pubkey.to_hex()
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::whitenoise::test_utils::*;

    #[tokio::test]
    async fn test_extract_pubkey_from_subscription_id() {
        let (whitenoise, _, _) = create_mock_whitenoise().await;
        let subscription_id = "abc123_user_events";
        let extracted = whitenoise
            .extract_pubkey_from_subscription_id(subscription_id)
            .await;
        assert!(extracted.is_err());

        let invalid_case = "no_underscore";
        let extracted = whitenoise
            .extract_pubkey_from_subscription_id(invalid_case)
            .await;
        assert!(extracted.is_err());

        let multi_underscore_id = "abc123_user_events_extra";
        let result = whitenoise
            .extract_pubkey_from_subscription_id(multi_underscore_id)
            .await;
        assert!(result.is_err());
    }
}

use std::sync::Arc;

use nostr_sdk::prelude::*;

use crate::{
    nostr_manager::utils::cap_timestamp_to_now,
    perf_instrument,
    relay_control::hash_pubkey_for_subscription_id,
    types::{EventSource, RetryInfo},
    whitenoise::{
        Whitenoise,
        accounts::Account,
        error::{Result, WhitenoiseError},
        session::AccountSession,
    },
};

impl Whitenoise {
    #[perf_instrument("event_processor")]
    pub(super) async fn process_account_event(
        &self,
        event: Event,
        source: EventSource,
        retry_info: RetryInfo,
    ) {
        // Get the account from the subscription ID, skip if we can't find it
        let account = match self.account_from_event_source(&source).await {
            Ok(account) => account,
            Err(e) => {
                tracing::debug!(
                    target: "whitenoise::event_processor::process_account_event",
                    "Skipping event {}: Cannot find account for event source: {}",
                    event.id.to_hex(),
                    e
                );
                return; // Skip - no retry
            }
        };

        // Resolve active session — if the account logged out between receipt
        // and processing, discard the event.
        let session = match self.session(&account.pubkey) {
            Some(s) => s,
            None => {
                tracing::debug!(
                    target: "whitenoise::event_processor::process_account_event",
                    "Skipping event {}: no active session for account {}",
                    event.id.to_hex(),
                    account.pubkey.to_hex()
                );
                return;
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
            .route_account_event_for_processing(&session, &account, &event)
            .await;

        // Handle the result - success, retry, or give up
        match result {
            Ok(()) => {
                // Record that we processed this event successfully
                if let Err(e) = self
                    .shared
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
                    Kind::ContactList | Kind::MlsGroupMessage | Kind::MuteList => {
                        // Cap timestamp to prevent future corruption
                        let safe_timestamp = cap_timestamp_to_now(event.created_at);
                        let created_ms = (safe_timestamp.as_secs() as i64) * 1000;

                        if let Err(e) = Account::update_last_synced_max(
                            &account.pubkey,
                            created_ms,
                            &self.shared.database,
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
                    // GiftWrap watermark advancement is handled inside
                    // handle_giftwrap so a single decrypt serves both
                    // Welcome processing and sync advancement.
                    _ => {}
                }
            }
            Err(WhitenoiseError::GiftwrapThrottled | WhitenoiseError::GiftwrapDeferred) => {
                // Deliberate defer (throttle budget exhausted or signer
                // backend transiently unavailable), not a failure. Do
                // NOT record as processed (relay replay must redeliver
                // once the bucket refills or the signer recovers) and
                // do NOT schedule a retry (in-process retry would hit
                // the same condition with no new information).
                tracing::debug!(
                    target: "whitenoise::event_processor::process_account_event",
                    account = %account.pubkey.to_hex(),
                    event_id = %event.id.to_hex(),
                    "Gift-wrap deferred; awaiting relay redelivery"
                );
            }
            Err(e) => {
                // Handle retry logic for actual processing errors
                if retry_info.should_retry() {
                    self.schedule_retry(event, source, retry_info, e);
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
    #[perf_instrument("event_processor")]
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
        let accounts = Account::all(&self.shared.database).await?;
        for account in accounts.iter() {
            let pubkey_hash = hash_pubkey_for_subscription_id(
                self.shared.relay_control.session_salt(),
                &account.pubkey,
            );
            if pubkey_hash == hash_str {
                return Ok(account.pubkey);
            }
        }

        Err(WhitenoiseError::InvalidEvent(format!(
            "No account found for subscription hash: {}",
            hash_str
        )))
    }

    #[perf_instrument("event_processor")]
    async fn account_from_event_source(&self, source: &EventSource) -> Result<Account> {
        let target_pubkey = match source {
            EventSource::RelaySubscription(context) => {
                context.account_pubkey.ok_or(WhitenoiseError::InvalidEvent(
                    "Relay subscription context missing account pubkey".to_string(),
                ))?
            }
        };

        tracing::debug!(
            target: "whitenoise::event_processor::account_from_event_source",
            "Processing account-scoped event for account: {}",
            target_pubkey.to_hex()
        );

        Account::find_by_pubkey(&target_pubkey, &self.shared.database).await
    }

    /// Check if an account event should be skipped (not processed)
    /// Returns Some(reason) if should skip, None if should process
    #[perf_instrument("event_processor")]
    async fn should_skip_account_event_processing(
        &self,
        event: &Event,
        account: &Account,
    ) -> Result<Option<&'static str>> {
        let already_processed = match self
            .shared
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
                .shared
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
    #[perf_instrument("event_processor")]
    async fn route_account_event_for_processing(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        event: &Event,
    ) -> Result<()> {
        match event.kind {
            Kind::GiftWrap => match validate_giftwrap_target(account, event) {
                Ok(()) => self.handle_giftwrap(session, account, event.clone()).await,
                Err(e) => Err(e),
            },
            Kind::MlsGroupMessage => {
                self.handle_mls_message(session, account, event.clone())
                    .await
            }
            Kind::Metadata => self.handle_metadata(event.clone()).await,
            Kind::RelayList | Kind::InboxRelays | Kind::MlsKeyPackageRelays => {
                self.handle_relay_list(event.clone()).await
            }
            Kind::ContactList => {
                self.handle_contact_list(session, account, event.clone())
                    .await
            }
            Kind::MuteList => self.handle_mute_list(account, event.clone()).await,
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
    use nostr_sdk::prelude::*;
    use sha2::{Digest, Sha256};

    use crate::relay_control::{RelayPlane, SubscriptionContext, SubscriptionStream};
    use crate::types::EventSource;
    use crate::whitenoise::accounts::Account;
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

    #[tokio::test]
    async fn test_extract_pubkey_from_subscription_id_no_underscore() {
        let (whitenoise, _, _) = create_mock_whitenoise().await;
        let result = whitenoise
            .extract_pubkey_from_subscription_id("nounderscore")
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid"));
    }

    #[tokio::test]
    async fn test_extract_pubkey_from_subscription_id_matches_account() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;

        // Create an account via the high-level API
        let account = whitenoise.create_identity().await.unwrap();

        // Build the expected subscription hash for this account
        let mut hasher = Sha256::new();
        hasher.update(whitenoise.shared.relay_control.session_salt());
        hasher.update(account.pubkey.to_bytes());
        let hash = hasher.finalize();
        let pubkey_hash = format!("{:x}", hash)[..12].to_string();

        let sub_id = format!("{}_user_events", pubkey_hash);
        let result = whitenoise
            .extract_pubkey_from_subscription_id(&sub_id)
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), account.pubkey);
    }

    #[tokio::test]
    async fn test_route_account_event_unhandled_kind() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let keys = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        // Use a kind that is not handled (e.g., TextNote)
        let event = EventBuilder::text_note("hello world")
            .sign(&keys)
            .await
            .unwrap();

        let result = whitenoise
            .route_account_event_for_processing(&session, &account, &event)
            .await;

        // Unhandled events return Ok(())
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_group_relay_fanout_preserves_account_scoped_processing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id = setup_two_member_group_with_accepted_account_groups(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;
        let admin_mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();
        let nostr_group_id = hex::encode(
            admin_mdk
                .get_group(&group_id)
                .unwrap()
                .unwrap()
                .nostr_group_id,
        );

        let mut inner = UnsignedEvent::new(
            admin_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "shared group relay event".to_string(),
        );
        inner.ensure_id();
        let event = admin_mdk.create_message(&group_id, inner, None).unwrap();
        let relay_url = RelayUrl::parse("ws://localhost:8080/").unwrap();

        for account in [&admin_account, &member_account] {
            whitenoise
                .process_account_event(
                    event.clone(),
                    EventSource::RelaySubscription(SubscriptionContext {
                        plane: RelayPlane::Group,
                        account_pubkey: Some(account.pubkey),
                        relay_url: relay_url.clone(),
                        stream: SubscriptionStream::GroupMessages,
                        group_ids: vec![nostr_group_id.clone()],
                    }),
                    Default::default(),
                )
                .await;
        }

        assert!(
            whitenoise
                .shared
                .event_tracker
                .already_processed_account_event(&event.id, &admin_account.pubkey)
                .await
                .unwrap(),
            "admin account should process the event under its own account context"
        );
        assert!(
            whitenoise
                .shared
                .event_tracker
                .already_processed_account_event(&event.id, &member_account.pubkey)
                .await
                .unwrap(),
            "member account should process the same wire event under its own account context"
        );
    }

    /// Verifies that events arriving on the `AccountMuteList` subscription
    /// stream are dispatched to `process_account_event` (i.e. they end up in
    /// the account-scoped handler pipeline).  We use a `Kind::MuteList` event
    /// authored and signed by the account so it passes the author guard inside
    /// `handle_mute_list`, and we confirm the event ID is recorded in the
    /// processed-events tracker after the call returns.
    #[tokio::test]
    async fn test_account_mute_list_stream_routes_to_account_event_processor() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let keys = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        let relay_url = RelayUrl::parse("ws://localhost:8080/").unwrap();

        // Build a syntactically valid but empty mute list event.
        let event = EventBuilder::new(Kind::MuteList, "")
            .sign(&keys)
            .await
            .unwrap();

        whitenoise
            .process_account_event(
                event.clone(),
                EventSource::RelaySubscription(SubscriptionContext {
                    plane: RelayPlane::AccountInbox,
                    account_pubkey: Some(account.pubkey),
                    relay_url: relay_url.clone(),
                    stream: SubscriptionStream::AccountMuteList,
                    group_ids: vec![],
                }),
                Default::default(),
            )
            .await;

        // The event must have been processed (not dropped or mis-routed).
        let was_processed = whitenoise
            .shared
            .event_tracker
            .already_processed_account_event(&event.id, &account.pubkey)
            .await
            .unwrap();
        assert!(
            was_processed,
            "AccountMuteList stream event should be tracked as processed"
        );
    }

    #[tokio::test]
    async fn test_validate_giftwrap_target_missing_p_tag() {
        let keys = Keys::generate();
        let account = Account {
            id: None,
            pubkey: keys.public_key(),
            user_id: 0,
            account_type: crate::whitenoise::accounts::AccountType::Local,
            last_synced_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        let event = EventBuilder::new(Kind::TextNote, "no p tag")
            .custom_created_at(Timestamp::now())
            .sign_with_keys(&keys)
            .unwrap();

        let result = super::validate_giftwrap_target(&account, &event);
        assert!(result.is_err(), "Missing p tag must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("No valid target pubkey"),
            "Error should describe missing p tag, got: {err_msg}"
        );
    }

    /// When the session is removed (account logged out) between event receipt
    /// and processing, the event is silently discarded and not tracked.
    #[tokio::test]
    async fn test_process_account_event_discards_when_session_missing() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let keys = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        // Build a contact-list event for this account
        let event = EventBuilder::new(Kind::ContactList, "")
            .sign(&keys)
            .await
            .unwrap();

        // Remove the session to simulate logout
        whitenoise.account_manager.remove_session(&account.pubkey);
        assert!(whitenoise.session(&account.pubkey).is_none());

        // Process the event; it should be silently discarded
        let relay_url = RelayUrl::parse("ws://localhost:8080/").unwrap();
        whitenoise
            .process_account_event(
                event.clone(),
                EventSource::RelaySubscription(SubscriptionContext {
                    plane: RelayPlane::Discovery,
                    account_pubkey: Some(account.pubkey),
                    relay_url,
                    stream: SubscriptionStream::DiscoveryFollowLists,
                    group_ids: Vec::new(),
                }),
                Default::default(),
            )
            .await;

        // Event must NOT be tracked as processed
        assert!(
            !whitenoise
                .shared
                .event_tracker
                .already_processed_account_event(&event.id, &account.pubkey)
                .await
                .unwrap(),
            "Event should not be tracked when session is missing"
        );
    }

    /// When the per-account gift-wrap throttle is exhausted, a validly-
    /// targeted kind:1059 event must NOT be recorded as processed.
    ///
    /// If it were, relay replay deduplication
    /// (`already_processed_account_event`) would silently drop the same
    /// event id on its next redelivery — defeating the throttle's deferral
    /// semantics and permanently losing legitimate Welcomes that arrived
    /// in the same burst as the spam.
    ///
    /// This test locks the contract in place: a future refactor that
    /// collapses `GiftwrapThrottled` back into the generic `Ok(())` /
    /// `Err(_)` paths will fail here.
    #[tokio::test]
    async fn test_throttled_giftwrap_is_not_recorded_as_processed() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let victim_account = whitenoise.create_identity().await.unwrap();
        let session = whitenoise.require_session(&victim_account.pubkey).unwrap();

        // Drain the bucket so the next handle_giftwrap returns GiftwrapThrottled.
        while session.giftwrap_throttle.try_acquire() {}

        // Build a kind:1059 event whose `p` tag points at the victim. The
        // content is irrelevant — the throttle fires before decryption is
        // attempted, so any bytes are fine.
        let attacker_keys = Keys::generate();
        let event = EventBuilder::new(Kind::GiftWrap, "ciphertext")
            .tag(Tag::public_key(victim_account.pubkey))
            .sign(&attacker_keys)
            .await
            .unwrap();

        let relay_url = RelayUrl::parse("ws://localhost:8080/").unwrap();
        whitenoise
            .process_account_event(
                event.clone(),
                EventSource::RelaySubscription(SubscriptionContext {
                    plane: RelayPlane::AccountInbox,
                    account_pubkey: Some(victim_account.pubkey),
                    relay_url,
                    stream: SubscriptionStream::AccountInboxGiftwraps,
                    group_ids: Vec::new(),
                }),
                Default::default(),
            )
            .await;

        assert!(
            !whitenoise
                .shared
                .event_tracker
                .already_processed_account_event(&event.id, &victim_account.pubkey)
                .await
                .unwrap(),
            "throttled gift-wrap must not be recorded as processed; \
             relay replay must be free to redeliver it after the bucket refills"
        );
    }

    /// An undecryptable gift-wrap MUST be marked processed and MUST consume
    /// exactly one throttle token — not eleven.
    ///
    /// Bogus or mis-targeted NIP-44 ciphertext will never decrypt no matter
    /// how many times we retry. Routing such failures through the generic
    /// `schedule_retry` path (max 10 attempts) would re-enqueue the same
    /// event and re-acquire a token on each attempt — letting a small burst
    /// of garbage gift-wraps drain the per-account decrypt budget many
    /// times over and starve legitimate Welcomes.
    ///
    /// This test pins the contract: a single bogus event consumes exactly
    /// one token and is recorded as processed so any subsequent redelivery
    /// is deduped.
    #[tokio::test]
    async fn test_undecryptable_giftwrap_is_terminal_not_retried() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let victim_account = whitenoise.create_identity().await.unwrap();
        let session = whitenoise.require_session(&victim_account.pubkey).unwrap();
        let tokens_before = session.giftwrap_throttle.tokens();

        // Build a kind:1059 event targeted at the victim, but with garbage
        // bytes that NIP-44 will fail to decrypt. The `p` tag is required
        // so the event clears `validate_giftwrap_target`.
        let attacker_keys = Keys::generate();
        let event = EventBuilder::new(Kind::GiftWrap, "not-a-valid-nip44-payload")
            .tag(Tag::public_key(victim_account.pubkey))
            .sign(&attacker_keys)
            .await
            .unwrap();

        let relay_url = RelayUrl::parse("ws://localhost:8080/").unwrap();
        whitenoise
            .process_account_event(
                event.clone(),
                EventSource::RelaySubscription(SubscriptionContext {
                    plane: RelayPlane::AccountInbox,
                    account_pubkey: Some(victim_account.pubkey),
                    relay_url,
                    stream: SubscriptionStream::AccountInboxGiftwraps,
                    group_ids: Vec::new(),
                }),
                Default::default(),
            )
            .await;

        assert!(
            whitenoise
                .shared
                .event_tracker
                .already_processed_account_event(&event.id, &victim_account.pubkey)
                .await
                .unwrap(),
            "undecryptable gift-wrap must be marked processed so subsequent \
             redeliveries (relay replay or in-process retry) are deduped"
        );

        // Refill during the processing window is bounded by wallclock and
        // capped at capacity, so the delta is essentially exactly 1.
        // Tolerance of ±0.2 absorbs scheduler jitter without admitting an
        // 11-token retry storm (which would show a delta of ~11).
        let tokens_after = session.giftwrap_throttle.tokens();
        let consumed = tokens_before - tokens_after;
        assert!(
            (0.8..=1.2).contains(&consumed),
            "decrypt-failed event should consume exactly one throttle token, \
             consumed {consumed} (before {tokens_before} → after {tokens_after}). \
             A value near 11 indicates the retry loop was reintroduced."
        );
    }

    /// External-signer transport failure during gift-wrap decryption MUST
    /// defer (not track, not retry) so relay replay can redeliver once the
    /// signer is reachable again.
    ///
    /// The library wraps deterministic local-crypto errors and transient
    /// external-signer transport errors identically as
    /// `nip59::Error::Signer(SignerError)`. The only way to distinguish
    /// them is by the signer's backend. This test exercises a non-Keys
    /// signer whose `nip44_decrypt` always fails and asserts that the
    /// event is deferred — neither recorded as processed nor scheduled
    /// for retry.
    #[tokio::test]
    async fn test_external_signer_decrypt_failure_is_deferred_not_terminal() {
        use std::pin::Pin;

        use nostr::SignerBackend;
        use nostr::signer::SignerError;
        use nostr::util::BoxedFuture;

        // Minimal mock NostrSigner whose backend is `Custom` (non-Keys)
        // and whose `nip44_decrypt` always returns an error. Other
        // methods are unreachable for this test path because
        // `extract_rumor` only ever calls `nip44_decrypt`.
        #[derive(Debug)]
        struct FailingExternalSigner;

        impl NostrSigner for FailingExternalSigner {
            fn backend(&self) -> SignerBackend {
                SignerBackend::Custom(std::borrow::Cow::Borrowed("test-failing-signer"))
            }
            fn get_public_key(&self) -> BoxedFuture<Result<PublicKey, SignerError>> {
                Box::pin(async { Err(SignerError::from("not implemented in test")) })
            }
            fn sign_event(
                &self,
                _unsigned: UnsignedEvent,
            ) -> BoxedFuture<Result<Event, SignerError>> {
                Box::pin(async { Err(SignerError::from("not implemented in test")) })
            }
            fn nip04_encrypt<'a>(
                &'a self,
                _public_key: &'a PublicKey,
                _content: &'a str,
            ) -> BoxedFuture<'a, Result<String, SignerError>> {
                Box::pin(async { Err(SignerError::from("not implemented in test")) })
            }
            fn nip04_decrypt<'a>(
                &'a self,
                _public_key: &'a PublicKey,
                _encrypted_content: &'a str,
            ) -> BoxedFuture<'a, Result<String, SignerError>> {
                Box::pin(async { Err(SignerError::from("not implemented in test")) })
            }
            fn nip44_encrypt<'a>(
                &'a self,
                _public_key: &'a PublicKey,
                _content: &'a str,
            ) -> BoxedFuture<'a, Result<String, SignerError>> {
                Box::pin(async { Err(SignerError::from("not implemented in test")) })
            }
            fn nip44_decrypt<'a>(
                &'a self,
                _public_key: &'a PublicKey,
                _payload: &'a str,
            ) -> Pin<Box<dyn std::future::Future<Output = Result<String, SignerError>> + Send + 'a>>
            {
                Box::pin(async { Err(SignerError::from("signer transport unavailable")) })
            }
        }

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let victim_account = whitenoise.create_identity().await.unwrap();
        let session = whitenoise.require_session(&victim_account.pubkey).unwrap();

        // Swap the session's signer for our failing external-style mock.
        session
            .set_signer(std::sync::Arc::new(FailingExternalSigner))
            .await;

        let tokens_before = session.giftwrap_throttle.tokens();

        // Build a kind:1059 event targeted at the victim. The content
        // doesn't matter — our mock signer fails on every nip44_decrypt.
        let attacker_keys = Keys::generate();
        let event = EventBuilder::new(Kind::GiftWrap, "any-ciphertext")
            .tag(Tag::public_key(victim_account.pubkey))
            .sign(&attacker_keys)
            .await
            .unwrap();

        let relay_url = RelayUrl::parse("ws://localhost:8080/").unwrap();
        whitenoise
            .process_account_event(
                event.clone(),
                EventSource::RelaySubscription(SubscriptionContext {
                    plane: RelayPlane::AccountInbox,
                    account_pubkey: Some(victim_account.pubkey),
                    relay_url,
                    stream: SubscriptionStream::AccountInboxGiftwraps,
                    group_ids: Vec::new(),
                }),
                Default::default(),
            )
            .await;

        // External-signer decrypt failure must NOT be recorded as
        // processed: relay replay needs to redeliver the event after
        // the signer recovers. Without this contract a transient
        // signer outage would permanently lose welcomes that landed
        // during the outage window.
        assert!(
            !whitenoise
                .shared
                .event_tracker
                .already_processed_account_event(&event.id, &victim_account.pubkey)
                .await
                .unwrap(),
            "external-signer transport failure must defer, not mark processed"
        );

        // One token consumed by the attempt — but not eleven. A value
        // near 11 would indicate the retry loop fired on this error
        // class, which would let a signer-disturbance attack amplify
        // bucket consumption.
        let tokens_after = session.giftwrap_throttle.tokens();
        let consumed = tokens_before - tokens_after;
        assert!(
            (0.8..=1.2).contains(&consumed),
            "deferred external-signer failure should consume exactly one token, \
             consumed {consumed} (before {tokens_before} → after {tokens_after})"
        );
    }
}

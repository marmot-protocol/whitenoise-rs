use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;
use tokio::sync::watch;

use crate::whitenoise::{
    Whitenoise,
    accounts::Account,
    database::processed_events::ProcessedEvent,
    error::{Result, WhitenoiseError},
    session::AccountSession,
    utils::timestamp_to_datetime,
};
use crate::{
    perf_instrument,
    relay_control::{RelayPlane, SubscriptionContext, SubscriptionStream},
    types::ProcessableEvent,
};

/// Maximum number of authors to include in one discovery catch-up query.
const CONTACT_LIST_CATCH_UP_BATCH_SIZE: usize = 500;

/// Timeout for each batched discovery catch-up query.
const CONTACT_LIST_CATCH_UP_TIMEOUT: Duration = Duration::from_secs(5);

impl Whitenoise {
    #[perf_instrument("event_handlers")]
    pub(crate) async fn handle_contact_list(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        event: Event,
    ) -> Result<()> {
        let _permit = session.acquire_contact_list_permit().await?;

        if self.should_skip_contact_list(session, &event).await? {
            return Ok(());
        }

        let contacts = crate::nostr_manager::utils::pubkeys_from_event(&event);
        let newly_created = account
            .update_follows_from_event(contacts.clone(), session, &self.shared.database)
            .await?;

        self.schedule_background_user_fetch(session, &contacts);

        self.shared
            .event_tracker
            .track_processed_account_event(&event, &account.pubkey)
            .await?;

        tracing::debug!(
            target: "whitenoise::handle_contact_list",
            "Processed contact list: {} contacts ({} new) for {}",
            contacts.len(),
            newly_created.len(),
            account.pubkey.to_hex()
        );

        Ok(())
    }

    #[perf_instrument("event_handlers")]
    async fn should_skip_contact_list(
        &self,
        session: &Arc<AccountSession>,
        event: &Event,
    ) -> Result<bool> {
        if ProcessedEvent::exists_for_account(&event.id, &session.account_db).await? {
            tracing::debug!(
                target: "whitenoise::handle_contact_list",
                "Skipping already processed event {}",
                event.id.to_hex()
            );
            return Ok(true);
        }

        if self.is_stale_contact_list(session, event).await? {
            return Ok(true);
        }

        Ok(false)
    }

    #[perf_instrument("event_handlers")]
    async fn is_stale_contact_list(
        &self,
        session: &Arc<AccountSession>,
        event: &Event,
    ) -> Result<bool> {
        let event_time = timestamp_to_datetime(event.created_at)?;
        let newest_time =
            ProcessedEvent::newest_contact_list_timestamp(&session.account_db).await?;

        match newest_time {
            Some(newest) if event_time.timestamp_millis() <= newest.timestamp_millis() => {
                tracing::debug!(
                    target: "whitenoise::handle_contact_list",
                    "Ignoring stale contact list (event: {}, newest: {})",
                    event_time.timestamp_millis(),
                    newest.timestamp_millis()
                );
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Spawns a single background task that refreshes discovery subscriptions
    /// and then catches up contact-list users in discovery-sized batches.
    ///
    /// This avoids the login bootstrap flood where each followed user triggers
    /// its own relay-list and metadata fetch workflow.
    fn schedule_background_user_fetch(&self, session: &Arc<AccountSession>, pubkeys: &[PublicKey]) {
        if pubkeys.is_empty() {
            return;
        }

        let pubkeys = pubkeys.to_vec();
        let total = pubkeys.len();

        let cancel_rx = Some(session.subscribe_cancellation());

        let Ok(whitenoise) = self.arc() else {
            tracing::error!(
                target: "whitenoise::handle_contact_list",
                "Whitenoise instance unavailable for background fetch"
            );
            return;
        };

        let tid = crate::perf::current_trace_id();
        tokio::spawn(crate::perf::with_trace_id(tid, async move {
            tracing::info!(
                target: "whitenoise::handle_contact_list",
                "Starting discovery catch-up for {} followed users",
                total,
            );

            whitenoise.shared.discovery_sync_worker.request_rebuild();

            let fetched = Self::fetch_users_batch(&whitenoise, &pubkeys, cancel_rx).await;

            tracing::info!(
                target: "whitenoise::handle_contact_list",
                "Discovery catch-up complete: {}/{} users queued for processing",
                fetched,
                total
            );
        }));
    }

    /// Fetches relay lists and metadata for a batch of users via the discovery
    /// plane. Returns the number of users whose catch-up work was queued.
    #[perf_instrument("event_handlers")]
    async fn fetch_users_batch(
        whitenoise: &Whitenoise,
        pubkeys: &[PublicKey],
        mut cancel_rx: Option<watch::Receiver<bool>>,
    ) -> usize {
        let mut unique_pubkeys = pubkeys.to_vec();
        unique_pubkeys.sort_unstable_by_key(|pubkey| pubkey.to_hex());
        unique_pubkeys.dedup();

        let Some(context_relay) = whitenoise
            .shared
            .relay_control
            .discovery()
            .relays()
            .first()
            .cloned()
        else {
            tracing::warn!(
                target: "whitenoise::handle_contact_list",
                "Skipping discovery catch-up because no discovery relays are configured"
            );
            return 0;
        };

        let mut queued_user_count = 0usize;

        for authors in unique_pubkeys.chunks(CONTACT_LIST_CATCH_UP_BATCH_SIZE) {
            let filter = Filter::new().authors(authors.to_vec()).kinds([
                Kind::Metadata,
                Kind::RelayList,
                Kind::InboxRelays,
                Kind::MlsKeyPackageRelays,
            ]);

            let batch_result = if let Some(cancel_rx) = cancel_rx.as_mut() {
                if *cancel_rx.borrow() {
                    tracing::debug!(
                        target: "whitenoise::handle_contact_list",
                        "Discovery catch-up cancelled, stopping"
                    );
                    None
                } else {
                    tokio::select! {
                        result = whitenoise
                            .shared.relay_control
                            .discovery()
                            .fetch_events(filter, CONTACT_LIST_CATCH_UP_TIMEOUT) => Some(result),
                        _ = cancel_rx.changed() => {
                            tracing::debug!(
                                target: "whitenoise::handle_contact_list",
                                "Discovery catch-up cancelled, stopping"
                            );
                            None
                        }
                    }
                }
            } else {
                Some(
                    whitenoise
                        .shared
                        .relay_control
                        .discovery()
                        .fetch_events(filter, CONTACT_LIST_CATCH_UP_TIMEOUT)
                        .await,
                )
            };

            let Some(batch_result) = batch_result else {
                break;
            };

            match batch_result {
                Ok(events) => {
                    if let Err(error) =
                        Self::queue_discovery_catch_up_events(whitenoise, &events, &context_relay)
                            .await
                    {
                        tracing::warn!(
                            target: "whitenoise::handle_contact_list",
                            "Failed to queue discovery catch-up events: {}",
                            error
                        );
                    } else {
                        queued_user_count += authors.len();
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        target: "whitenoise::handle_contact_list",
                        "Discovery catch-up query failed for batch of {} users: {}",
                        authors.len(),
                        error
                    );
                }
            }
        }

        queued_user_count
    }

    #[perf_instrument("event_handlers")]
    async fn queue_discovery_catch_up_events(
        whitenoise: &Whitenoise,
        events: &Events,
        relay_url: &RelayUrl,
    ) -> Result<()> {
        let source = SubscriptionContext {
            plane: RelayPlane::Discovery,
            account_pubkey: None,
            relay_url: relay_url.clone(),
            stream: SubscriptionStream::DiscoveryUserData,
            group_ids: Vec::new(),
        };

        for event in events.iter() {
            whitenoise
                .event_sender
                .send(ProcessableEvent::new_routed_nostr_event(
                    event.clone(),
                    source.clone(),
                ))
                .await
                .map_err(|error| {
                    WhitenoiseError::EventProcessor(format!(
                        "Failed to enqueue discovery catch-up event: {error}"
                    ))
                })?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::prelude::*;

    use crate::whitenoise::{
        accounts::{Account, AccountType},
        database::processed_events::ProcessedEvent,
        test_utils::*,
        utils::timestamp_to_datetime,
    };

    /// Helper to build a contact list event (Kind 3) with specified contacts
    async fn build_contact_list_event(
        signer: &Keys,
        contacts: &[PublicKey],
        timestamp: Option<Timestamp>,
    ) -> Event {
        let tags: Vec<Tag> = contacts
            .iter()
            .map(|pk| Tag::custom(TagKind::p(), [pk.to_hex()]))
            .collect();

        let mut builder = EventBuilder::new(Kind::ContactList, "").tags(tags);
        if let Some(ts) = timestamp {
            builder = builder.custom_created_at(ts);
        }
        builder.sign(signer).await.unwrap()
    }

    #[tokio::test]
    async fn test_handle_contact_list_success() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let keys = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        let contact1 = Keys::generate().public_key();
        let contact2 = Keys::generate().public_key();
        // Include duplicates to verify deduplication
        let contacts = vec![contact1, contact2, contact1];
        let timestamp = Timestamp::from(1700000000u64);

        let event = build_contact_list_event(&keys, &contacts, Some(timestamp)).await;

        whitenoise
            .handle_contact_list(&session, &account, event.clone())
            .await
            .unwrap();

        // Verify follows were created and deduplicated
        let follows = session.repos.follows.all().await.unwrap();
        assert_eq!(follows.len(), 2, "Duplicates should be deduplicated");

        let follow_pubkeys: Vec<PublicKey> = follows.iter().map(|u| u.pubkey).collect();
        assert!(follow_pubkeys.contains(&contact1));
        assert!(follow_pubkeys.contains(&contact2));

        // Verify event was tracked as processed
        assert!(
            ProcessedEvent::exists_for_account(&event.id, &session.account_db)
                .await
                .unwrap()
        );

        // Verify timestamp was recorded for future ordering checks
        let newest_timestamp = ProcessedEvent::newest_contact_list_timestamp(&session.account_db)
            .await
            .unwrap()
            .unwrap();
        let expected = timestamp_to_datetime(timestamp).unwrap();
        assert_eq!(
            newest_timestamp.timestamp_millis(),
            expected.timestamp_millis()
        );
    }

    #[tokio::test]
    async fn test_handle_contact_list_idempotency() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let keys = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        let contact = Keys::generate().public_key();
        let event = build_contact_list_event(&keys, &[contact], None).await;

        // Process the same event twice
        whitenoise
            .handle_contact_list(&session, &account, event.clone())
            .await
            .unwrap();
        whitenoise
            .handle_contact_list(&session, &account, event)
            .await
            .unwrap();

        // Should still have exactly 1 follow
        let follows = session.repos.follows.all().await.unwrap();
        assert_eq!(follows.len(), 1);
    }

    #[tokio::test]
    async fn test_handle_contact_list_stale_events_ignored() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let keys = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        let current_contact = Keys::generate().public_key();
        let older_contact = Keys::generate().public_key();
        let same_ts_contact = Keys::generate().public_key();

        let current_timestamp = Timestamp::now();
        let older_timestamp = Timestamp::from(current_timestamp.as_secs() - 3600);

        // Process the "current" event
        let current_event =
            build_contact_list_event(&keys, &[current_contact], Some(current_timestamp)).await;
        whitenoise
            .handle_contact_list(&session, &account, current_event)
            .await
            .unwrap();

        // Try processing an older event - should be ignored
        let older_event =
            build_contact_list_event(&keys, &[older_contact], Some(older_timestamp)).await;
        whitenoise
            .handle_contact_list(&session, &account, older_event)
            .await
            .unwrap();

        let follows = session.repos.follows.all().await.unwrap();
        assert_eq!(follows.len(), 1);
        assert_eq!(follows[0].pubkey, current_contact, "Older event ignored");

        // Try processing an event with the same timestamp - should also be ignored
        let same_ts_event =
            build_contact_list_event(&keys, &[same_ts_contact], Some(current_timestamp)).await;
        whitenoise
            .handle_contact_list(&session, &account, same_ts_event)
            .await
            .unwrap();

        let follows = session.repos.follows.all().await.unwrap();
        assert_eq!(follows.len(), 1);
        assert_eq!(
            follows[0].pubkey, current_contact,
            "Same-timestamp event ignored"
        );
    }

    #[tokio::test]
    async fn test_handle_contact_list_newer_event_replaces() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let keys = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        let first_contact = Keys::generate().public_key();
        let second_contact = Keys::generate().public_key();

        let t1 = Timestamp::from(Timestamp::now().as_secs() - 3600);
        let t2 = Timestamp::now();
        let t3 = Timestamp::from(Timestamp::now().as_secs() + 1);

        // Process first event
        let first_event = build_contact_list_event(&keys, &[first_contact], Some(t1)).await;
        whitenoise
            .handle_contact_list(&session, &account, first_event)
            .await
            .unwrap();
        assert_eq!(session.repos.follows.all().await.unwrap().len(), 1);

        // Process newer event - should replace
        let second_event = build_contact_list_event(&keys, &[second_contact], Some(t2)).await;
        whitenoise
            .handle_contact_list(&session, &account, second_event)
            .await
            .unwrap();

        let follows = session.repos.follows.all().await.unwrap();
        assert_eq!(follows.len(), 1);
        assert_eq!(follows[0].pubkey, second_contact);

        // Process empty list - should clear all follows
        let empty_event = build_contact_list_event(&keys, &[], Some(t3)).await;
        whitenoise
            .handle_contact_list(&session, &account, empty_event)
            .await
            .unwrap();

        assert!(
            session.repos.follows.all().await.unwrap().is_empty(),
            "Empty contact list should clear follows"
        );
    }

    #[tokio::test]
    async fn test_handle_contact_list_accounts_are_independent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account1 = whitenoise.create_identity().await.unwrap();
        let session1 = whitenoise.require_session(&account1.pubkey).unwrap();
        let keys1 = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&account1.pubkey)
            .unwrap();

        let account2 = whitenoise.create_identity().await.unwrap();
        let session2 = whitenoise.require_session(&account2.pubkey).unwrap();
        let keys2 = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&account2.pubkey)
            .unwrap();

        let contact1 = Keys::generate().public_key();
        let contact2 = Keys::generate().public_key();

        whitenoise
            .handle_contact_list(
                &session1,
                &account1,
                build_contact_list_event(&keys1, &[contact1], None).await,
            )
            .await
            .unwrap();
        whitenoise
            .handle_contact_list(
                &session2,
                &account2,
                build_contact_list_event(&keys2, &[contact2], None).await,
            )
            .await
            .unwrap();

        let follows1 = session1.repos.follows.all().await.unwrap();
        let follows2 = session2.repos.follows.all().await.unwrap();

        assert_eq!(follows1.len(), 1);
        assert_eq!(follows2.len(), 1);
        assert_eq!(follows1[0].pubkey, contact1);
        assert_eq!(follows2[0].pubkey, contact2);
    }

    /// Verifies that an Account with `id: None` still triggers an error even
    /// when a valid session exists. The handler checks `account.id` early and
    /// returns `AccountNotFound` before doing any work.
    #[tokio::test]
    async fn test_handle_contact_list_requires_saved_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create a real identity (which has a session) then fabricate an
        // Account struct with id: None to simulate the unsaved-account path.
        let real_account = whitenoise.create_identity().await.unwrap();
        let session = whitenoise.require_session(&real_account.pubkey).unwrap();

        let unsaved_account = Account {
            id: None,
            pubkey: real_account.pubkey,
            user_id: 0,
            account_type: AccountType::Local,
            last_synced_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        let keys = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&real_account.pubkey)
            .unwrap();
        let event = build_contact_list_event(&keys, &[Keys::generate().public_key()], None).await;

        assert!(
            whitenoise
                .handle_contact_list(&session, &unsaved_account, event)
                .await
                .is_err()
        );
    }
}

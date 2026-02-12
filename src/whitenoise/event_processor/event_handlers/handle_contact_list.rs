use std::sync::Arc;

use futures::stream::{self, StreamExt};
use nostr_sdk::prelude::*;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use crate::{
    nostr_manager::NostrManager,
    whitenoise::{
        Whitenoise,
        accounts::Account,
        database::processed_events::ProcessedEvent,
        error::{Result, WhitenoiseError},
        users::User,
        utils::timestamp_to_datetime,
    },
};

/// Maximum number of concurrent user data fetches when processing a contact list.
/// Each fetch does ~4 network round-trips (3 relay list + 1 metadata), so 35
/// concurrent fetches means ~140 in-flight requests — well within the relay
/// pool's internal channel capacity (1024) while balancing login speed against
/// follow list population time.
const CONTACT_LIST_FETCH_CONCURRENCY: usize = 35;

impl Whitenoise {
    pub(crate) async fn handle_contact_list(&self, account: &Account, event: Event) -> Result<()> {
        let _permit = self.acquire_contact_list_guard(account).await?;
        let account_id = account.id.ok_or(WhitenoiseError::AccountNotFound)?;

        if self.should_skip_contact_list(&event, account_id).await? {
            return Ok(());
        }

        let contacts = NostrManager::pubkeys_from_event(&event);
        let newly_created = account
            .update_follows_from_event(contacts.clone(), &self.database)
            .await?;

        self.schedule_background_user_fetch(&newly_created, &account.pubkey);

        self.nostr
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

    async fn acquire_contact_list_guard(&self, account: &Account) -> Result<OwnedSemaphorePermit> {
        let semaphore = self
            .contact_list_guards
            .entry(account.pubkey)
            .or_insert_with(|| Arc::new(Semaphore::new(1)))
            .clone();

        semaphore.acquire_owned().await.map_err(|_| {
            WhitenoiseError::ContactList(
                "Failed to acquire contact list processing permit".to_string(),
            )
        })
    }

    async fn should_skip_contact_list(&self, event: &Event, account_id: i64) -> Result<bool> {
        if ProcessedEvent::exists(&event.id, Some(account_id), &self.database).await? {
            tracing::debug!(
                target: "whitenoise::handle_contact_list",
                "Skipping already processed event {}",
                event.id.to_hex()
            );
            return Ok(true);
        }

        if self.is_stale_contact_list(event, account_id).await? {
            return Ok(true);
        }

        Ok(false)
    }

    async fn is_stale_contact_list(&self, event: &Event, account_id: i64) -> Result<bool> {
        let event_time = timestamp_to_datetime(event.created_at)?;
        let newest_time =
            ProcessedEvent::newest_contact_list_timestamp(account_id, &self.database).await?;

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

    /// Spawns a single background task that fetches relay lists and metadata for
    /// newly discovered users from a contact list, with bounded concurrency and
    /// cancellation support.
    ///
    /// Instead of spawning one unbounded task per user (which overwhelms the
    /// relay pool), this processes users in a stream with at most
    /// [`CONTACT_LIST_FETCH_CONCURRENCY`] fetches in flight at any time.
    ///
    /// `refresh_global_subscription_for_user` is called once after the entire
    /// batch completes, rather than per-user.
    fn schedule_background_user_fetch(&self, pubkeys: &[PublicKey], account_pubkey: &PublicKey) {
        if pubkeys.is_empty() {
            return;
        }

        let pubkeys = pubkeys.to_vec();
        let total = pubkeys.len();

        // Get a cancellation receiver for this account so the batch stops if
        // the account logs out.
        let cancel_rx = self
            .background_task_cancellation
            .get(account_pubkey)
            .map(|entry| entry.value().subscribe());

        tokio::spawn(async move {
            let whitenoise = match Whitenoise::get_instance() {
                Ok(wn) => wn,
                Err(e) => {
                    tracing::error!(
                        target: "whitenoise::handle_contact_list",
                        "Failed to get Whitenoise instance for background fetch: {}",
                        e
                    );
                    return;
                }
            };

            tracing::info!(
                target: "whitenoise::handle_contact_list",
                "Starting background fetch for {} new users (concurrency: {})",
                total,
                CONTACT_LIST_FETCH_CONCURRENCY
            );

            let fetched = Self::fetch_users_batch(whitenoise, &pubkeys, cancel_rx).await;

            tracing::info!(
                target: "whitenoise::handle_contact_list",
                "Background fetch complete: {}/{} users fetched",
                fetched,
                total
            );

            // Refresh all global subscriptions once after all users are
            // processed. This refreshes every batch on every relay so that
            // newly discovered users (who may be spread across many different
            // relay batches) are properly subscribed to.
            if fetched > 0
                && let Err(e) = whitenoise.refresh_all_global_subscriptions().await
            {
                tracing::warn!(
                    target: "whitenoise::handle_contact_list",
                    "Failed to refresh global subscriptions after batch fetch: {}",
                    e
                );
            }
        });
    }

    /// Fetches relay lists and metadata for a batch of users with bounded
    /// concurrency. Returns the number of users successfully fetched.
    ///
    /// Checks the cancellation signal before starting each user fetch and
    /// stops early if cancelled (e.g. on logout).
    async fn fetch_users_batch(
        whitenoise: &Whitenoise,
        pubkeys: &[PublicKey],
        cancel_rx: Option<watch::Receiver<bool>>,
    ) -> usize {
        let results: Vec<bool> = stream::iter(pubkeys.iter().copied())
            .map(|pubkey| {
                let cancel_rx = cancel_rx.clone();
                async move {
                    // Check cancellation before starting this user's fetch
                    if let Some(rx) = &cancel_rx
                        && *rx.borrow()
                    {
                        tracing::debug!(
                            target: "whitenoise::handle_contact_list",
                            "Background fetch cancelled, stopping"
                        );
                        return false;
                    }

                    let user =
                        match User::find_or_create_by_pubkey(&pubkey, &whitenoise.database).await {
                            Ok((user, _)) => user,
                            Err(e) => {
                                tracing::warn!(
                                    target: "whitenoise::handle_contact_list",
                                    "Failed to find/create user {}: {}",
                                    pubkey.to_hex(),
                                    e
                                );
                                return false;
                            }
                        };

                    // Fetch relay lists then metadata (serial per user since
                    // metadata fetch benefits from knowing the user's relays)
                    if let Err(e) = user.update_relay_lists(whitenoise).await {
                        tracing::warn!(
                            target: "whitenoise::handle_contact_list",
                            "Failed to fetch relay lists for {}: {}",
                            pubkey.to_hex(),
                            e
                        );
                    }

                    let mut user_for_metadata = user.clone();
                    if let Err(e) = user_for_metadata.sync_metadata(whitenoise).await {
                        tracing::warn!(
                            target: "whitenoise::handle_contact_list",
                            "Failed to fetch metadata for {}: {}",
                            pubkey.to_hex(),
                            e
                        );
                    }

                    true
                }
            })
            .buffer_unordered(CONTACT_LIST_FETCH_CONCURRENCY)
            .collect()
            .await;

        results.iter().filter(|&&ok| ok).count()
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
        let keys = whitenoise
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
            .handle_contact_list(&account, event.clone())
            .await
            .unwrap();

        // Verify follows were created and deduplicated
        let follows = account.follows(&whitenoise.database).await.unwrap();
        assert_eq!(follows.len(), 2, "Duplicates should be deduplicated");

        let follow_pubkeys: Vec<PublicKey> = follows.iter().map(|u| u.pubkey).collect();
        assert!(follow_pubkeys.contains(&contact1));
        assert!(follow_pubkeys.contains(&contact2));

        // Verify event was tracked as processed
        let account_id = account.id.unwrap();
        assert!(
            ProcessedEvent::exists(&event.id, Some(account_id), &whitenoise.database)
                .await
                .unwrap()
        );

        // Verify timestamp was recorded for future ordering checks
        let newest_timestamp =
            ProcessedEvent::newest_contact_list_timestamp(account_id, &whitenoise.database)
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
        let keys = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        let contact = Keys::generate().public_key();
        let event = build_contact_list_event(&keys, &[contact], None).await;

        // Process the same event twice
        whitenoise
            .handle_contact_list(&account, event.clone())
            .await
            .unwrap();
        whitenoise
            .handle_contact_list(&account, event)
            .await
            .unwrap();

        // Should still have exactly 1 follow
        let follows = account.follows(&whitenoise.database).await.unwrap();
        assert_eq!(follows.len(), 1);
    }

    #[tokio::test]
    async fn test_handle_contact_list_stale_events_ignored() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let keys = whitenoise
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
            .handle_contact_list(&account, current_event)
            .await
            .unwrap();

        // Try processing an older event - should be ignored
        let older_event =
            build_contact_list_event(&keys, &[older_contact], Some(older_timestamp)).await;
        whitenoise
            .handle_contact_list(&account, older_event)
            .await
            .unwrap();

        let follows = account.follows(&whitenoise.database).await.unwrap();
        assert_eq!(follows.len(), 1);
        assert_eq!(follows[0].pubkey, current_contact, "Older event ignored");

        // Try processing an event with the same timestamp - should also be ignored
        let same_ts_event =
            build_contact_list_event(&keys, &[same_ts_contact], Some(current_timestamp)).await;
        whitenoise
            .handle_contact_list(&account, same_ts_event)
            .await
            .unwrap();

        let follows = account.follows(&whitenoise.database).await.unwrap();
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
        let keys = whitenoise
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
            .handle_contact_list(&account, first_event)
            .await
            .unwrap();
        assert_eq!(
            account.follows(&whitenoise.database).await.unwrap().len(),
            1
        );

        // Process newer event - should replace
        let second_event = build_contact_list_event(&keys, &[second_contact], Some(t2)).await;
        whitenoise
            .handle_contact_list(&account, second_event)
            .await
            .unwrap();

        let follows = account.follows(&whitenoise.database).await.unwrap();
        assert_eq!(follows.len(), 1);
        assert_eq!(follows[0].pubkey, second_contact);

        // Process empty list - should clear all follows
        let empty_event = build_contact_list_event(&keys, &[], Some(t3)).await;
        whitenoise
            .handle_contact_list(&account, empty_event)
            .await
            .unwrap();

        assert!(
            account
                .follows(&whitenoise.database)
                .await
                .unwrap()
                .is_empty(),
            "Empty contact list should clear follows"
        );
    }

    #[tokio::test]
    async fn test_handle_contact_list_accounts_are_independent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account1 = whitenoise.create_identity().await.unwrap();
        let keys1 = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&account1.pubkey)
            .unwrap();

        let account2 = whitenoise.create_identity().await.unwrap();
        let keys2 = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&account2.pubkey)
            .unwrap();

        let contact1 = Keys::generate().public_key();
        let contact2 = Keys::generate().public_key();

        whitenoise
            .handle_contact_list(
                &account1,
                build_contact_list_event(&keys1, &[contact1], None).await,
            )
            .await
            .unwrap();
        whitenoise
            .handle_contact_list(
                &account2,
                build_contact_list_event(&keys2, &[contact2], None).await,
            )
            .await
            .unwrap();

        let follows1 = account1.follows(&whitenoise.database).await.unwrap();
        let follows2 = account2.follows(&whitenoise.database).await.unwrap();

        assert_eq!(follows1.len(), 1);
        assert_eq!(follows2.len(), 1);
        assert_eq!(follows1[0].pubkey, contact1);
        assert_eq!(follows2[0].pubkey, contact2);
    }

    #[tokio::test]
    async fn test_handle_contact_list_requires_saved_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys = Keys::generate();
        let unsaved_account = Account {
            id: None,
            pubkey: keys.public_key(),
            user_id: 0,
            account_type: AccountType::Local,
            last_synced_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        let event = build_contact_list_event(&keys, &[Keys::generate().public_key()], None).await;

        assert!(
            whitenoise
                .handle_contact_list(&unsaved_account, event)
                .await
                .is_err()
        );
    }
}

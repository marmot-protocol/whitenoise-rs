use nostr_sdk::prelude::*;

use crate::{
    perf_instrument,
    whitenoise::{
        Whitenoise,
        accounts::Account,
        database::processed_events::ProcessedEvent,
        error::{Result, WhitenoiseError},
        users::User,
        utils::timestamp_to_datetime,
    },
};

impl Whitenoise {
    #[perf_instrument("event_handlers")]
    pub async fn handle_relay_list(&self, event: Event) -> Result<()> {
        // Check if we've already processed this specific event from this author
        let already_processed = ProcessedEvent::exists(
            &event.id,
            None, // Global events (relay lists)
            &self.database,
        )
        .await?;

        if already_processed {
            tracing::debug!(
                target: "whitenoise::event_processor::handle_relay_list",
                "Skipping already processed relay list event {} from author {}",
                event.id.to_hex(),
                event.pubkey.to_hex()
            );
            return Ok(());
        }

        let (user, _newly_created) =
            User::find_or_create_by_pubkey(&event.pubkey, &self.database).await?;

        let relay_type = event.kind.into();
        let relay_urls = crate::nostr_manager::utils::relay_urls_from_event(&event);
        let event_created_at = Some(timestamp_to_datetime(event.created_at)?);
        let relays_changed = user
            .sync_relay_urls(self, relay_type, &relay_urls, event_created_at)
            .await?;

        if relays_changed {
            self.handle_subscriptions_refresh(&user, &event).await;
        }

        // Track this processed event
        ProcessedEvent::create(
            &event.id,
            None, // Global events (relay lists)
            event_created_at,
            Some(event.kind),
            Some(&event.pubkey),
            &self.database,
        )
        .await?;

        Ok(())
    }

    #[perf_instrument("event_handlers")]
    async fn handle_subscriptions_refresh(&self, user: &User, event: &Event) {
        let user_pubkey = user.pubkey;
        let event_pubkey = event.pubkey;
        let tid = crate::perf::current_trace_id();

        tokio::spawn(crate::perf::with_trace_id(tid, async move {
            let whitenoise = match Whitenoise::get_instance() {
                Ok(instance) => instance,
                Err(error) => {
                    tracing::warn!(
                        target: "whitenoise::handle_relay_list",
                        "Failed to get Whitenoise instance for relay list refresh {}: {}",
                        event_pubkey,
                        error
                    );
                    return;
                }
            };

            let account = match Account::find_by_pubkey(&user_pubkey, &whitenoise.database).await {
                Ok(account) => Some(account),
                Err(WhitenoiseError::AccountNotFound) => None,
                Err(error) => {
                    tracing::warn!(
                        target: "whitenoise::handle_relay_list",
                        "Failed to look up account for relay list refresh {}: {}",
                        event_pubkey,
                        error
                    );
                    None
                }
            };

            whitenoise.discovery_sync_worker.request_rebuild();

            if let Some(account) = account
                && let Err(error) = whitenoise.refresh_account_subscriptions(&account).await
            {
                tracing::warn!(
                    target: "whitenoise::handle_relay_list",
                    "Failed to refresh account subscriptions after relay list change for {}: {}",
                    event_pubkey,
                    error
                );
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::prelude::*;

    use crate::whitenoise::{
        database::processed_events::ProcessedEvent, relays::RelayType,
        test_utils::create_mock_whitenoise,
    };

    async fn relay_list_event(keys: &Keys, relay_urls: &[&str]) -> Event {
        let tags: Vec<Tag> = relay_urls.iter().map(|url| Tag::reference(*url)).collect();
        EventBuilder::new(Kind::RelayList, "")
            .tags(tags)
            .sign(keys)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn relay_list_event_creates_user_with_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        let event = relay_list_event(
            &keys,
            &["wss://relay1.example.com", "wss://relay2.example.com"],
        )
        .await;
        whitenoise.handle_relay_list(event).await.unwrap();

        let user = whitenoise
            .find_user_by_pubkey(&keys.public_key())
            .await
            .unwrap();
        let relays = user
            .relays(RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(relays.len(), 2);
    }

    #[tokio::test]
    async fn duplicate_relay_list_event_is_idempotent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        let event = relay_list_event(&keys, &["wss://relay1.example.com"]).await;
        whitenoise.handle_relay_list(event.clone()).await.unwrap();
        whitenoise.handle_relay_list(event.clone()).await.unwrap();

        // Event should be recorded exactly once
        let processed = ProcessedEvent::exists(&event.id, None, &whitenoise.database)
            .await
            .unwrap();
        assert!(processed);

        // User should still have the correct relays
        let user = whitenoise
            .find_user_by_pubkey(&keys.public_key())
            .await
            .unwrap();
        let relays = user
            .relays(RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(relays.len(), 1);
    }
}

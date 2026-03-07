use nostr_sdk::prelude::*;

use crate::{
    nostr_manager::NostrManager,
    whitenoise::{
        Whitenoise, accounts::Account, database::processed_events::ProcessedEvent, error::Result,
        users::User, utils::timestamp_to_datetime,
    },
};

impl Whitenoise {
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
        let relay_urls = NostrManager::relay_urls_from_event(&event);
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

    async fn handle_subscriptions_refresh(&self, user: &User, event: &Event) {
        // Refresh global subscriptions for this user (metadata, relay lists, key packages)
        if let Err(e) = self.refresh_global_subscription_for_user().await {
            tracing::warn!(
                target: "whitenoise::handle_relay_list",
                "Failed to refresh global subscriptions after relay list change for {}: {}",
                event.pubkey, e
            );
        }

        // If there's an account for this user, also refresh their account subscriptions
        if let Ok(account) = Account::find_by_pubkey(&user.pubkey, &self.database).await
            && let Err(e) = self.refresh_account_subscriptions(&account).await
        {
            tracing::warn!(
                target: "whitenoise::handle_relay_list",
                "Failed to refresh account subscriptions after relay list change for {}: {}",
                event.pubkey, e
            );
        }
    }
}

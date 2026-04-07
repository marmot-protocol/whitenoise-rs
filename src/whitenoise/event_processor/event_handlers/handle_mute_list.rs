use nostr_sdk::prelude::*;

use crate::perf_instrument;
use crate::whitenoise::{Whitenoise, accounts::Account, error::Result};

impl Whitenoise {
    /// Handles an incoming kind 10000 mute list event (our own, from another
    /// device or echoed back from a relay). Decrypts the private section and
    /// syncs the local cache.
    #[perf_instrument("event_handlers")]
    pub(crate) async fn handle_mute_list(&self, account: &Account, event: Event) -> Result<()> {
        // Only process mute list events authored by this account
        if event.pubkey != account.pubkey {
            tracing::debug!(
                target: "whitenoise::event_processor::handle_mute_list",
                "Ignoring mute list event from foreign author {}",
                event.pubkey,
            );
            return Ok(());
        }

        let signer = self.get_signer_for_account(account)?;

        let entries = Self::parse_mute_list_entries(signer.as_ref(), &account.pubkey, &event).await;
        let Some(entries) = entries else {
            return Ok(());
        };

        self.sync_and_emit(account, &entries).await?;

        tracing::debug!(
            target: "whitenoise::event_processor::handle_mute_list",
            "Synced mute list for {}: {} entries",
            account.pubkey,
            entries.len(),
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::{EventBuilder, Keys, Kind, Tag};

    use crate::whitenoise::{
        database::mute_list::MuteListEntry, test_utils::create_mock_whitenoise,
    };

    #[tokio::test]
    async fn handle_mute_list_foreign_author_ignored() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let foreign_keys = Keys::generate();
        let target = Keys::generate().public_key();

        // Event authored by a foreign key, not the account
        let event = EventBuilder::new(Kind::MuteList, "")
            .tags([Tag::public_key(target)])
            .sign(&foreign_keys)
            .await
            .unwrap();

        let result = whitenoise.handle_mute_list(&account, event).await;
        assert!(
            result.is_ok(),
            "foreign author event should be silently ignored"
        );

        // Cache must remain empty — the foreign event must not have been applied
        let blocked = whitenoise.get_blocked_users(&account).await.unwrap();
        assert!(
            blocked.is_empty(),
            "foreign mute list event must not modify the local cache"
        );
    }

    #[tokio::test]
    async fn handle_mute_list_own_event_syncs_public_entries() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let target = Keys::generate().public_key();

        // Build the event using the account's own keys
        let account_keys = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        let event = EventBuilder::new(Kind::MuteList, "")
            .tags([Tag::public_key(target)])
            .sign(&account_keys)
            .await
            .unwrap();

        let result = whitenoise.handle_mute_list(&account, event).await;
        assert!(result.is_ok());

        assert!(
            MuteListEntry::exists(&account.pubkey, &target, &whitenoise.database)
                .await
                .unwrap(),
            "public tag from own mute list event must be cached"
        );
    }

    #[tokio::test]
    async fn handle_mute_list_bad_content_leaves_cache_unchanged() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let pre_existing = Keys::generate().public_key();

        // Pre-populate the cache
        MuteListEntry::insert(&account.pubkey, &pre_existing, true, &whitenoise.database)
            .await
            .unwrap();

        let account_keys = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        // Event with non-empty content that cannot be decrypted
        let event = EventBuilder::new(Kind::MuteList, "not-valid-ciphertext")
            .sign(&account_keys)
            .await
            .unwrap();

        let result = whitenoise.handle_mute_list(&account, event).await;
        assert!(result.is_ok(), "decrypt failure must not return an error");

        // Cache must be unchanged — parse failure returns None and we skip sync
        assert!(
            MuteListEntry::exists(&account.pubkey, &pre_existing, &whitenoise.database)
                .await
                .unwrap(),
            "pre-existing entry must survive a failed decrypt"
        );
    }
}

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

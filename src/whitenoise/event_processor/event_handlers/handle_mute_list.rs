use nostr_sdk::prelude::*;

use crate::perf_instrument;
use crate::whitenoise::{
    Whitenoise, accounts::Account, database::mute_list::MuteListEntry, error::Result,
};

impl Whitenoise {
    /// Handles an incoming kind 10000 mute list event (our own, from another
    /// device or echoed back from a relay). Decrypts the private section and
    /// syncs the local cache.
    #[perf_instrument("event_handlers")]
    pub(crate) async fn handle_mute_list(&self, account: &Account, event: Event) -> Result<()> {
        let signer = self.get_signer_for_account(account)?;

        let mut entries: Vec<(PublicKey, bool)> = Vec::new();

        // Public tags: extract "p" tags
        for tag in event.tags.iter() {
            if let Some(TagStandard::PublicKey { public_key, .. }) = tag.as_standardized() {
                entries.push((*public_key, false));
            }
        }

        // Private content: decrypt with NIP-44 via signer and extract "p" tags
        if !event.content.is_empty() {
            match signer.nip44_decrypt(&account.pubkey, &event.content).await {
                Ok(decrypted) => {
                    if let Ok(tags) = serde_json::from_str::<Vec<Vec<String>>>(&decrypted) {
                        for tag in &tags {
                            if tag.len() >= 2
                                && tag[0] == "p"
                                && let Ok(pk) = PublicKey::parse(&tag[1])
                            {
                                entries.push((pk, true));
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::event_handlers::handle_mute_list",
                        "Failed to decrypt mute list content: {}",
                        e,
                    );
                }
            }
        }

        MuteListEntry::sync_from_event(&account.pubkey, &entries, &self.database).await?;

        tracing::debug!(
            target: "whitenoise::event_handlers::handle_mute_list",
            "Synced mute list for {}: {} entries",
            account.pubkey,
            entries.len(),
        );

        Ok(())
    }
}

use nostr_sdk::{EventBuilder, Filter, Kind, PublicKey, Tag, TagStandard};

use crate::perf_instrument;
use crate::whitenoise::{
    Whitenoise,
    accounts::Account,
    accounts_groups::AccountGroup,
    chat_list_streaming::ChatListUpdateTrigger,
    database::mute_list::MuteListEntry,
    error::{Result, WhitenoiseError},
};

impl Whitenoise {
    /// Blocks a user by adding them to the local mute list cache and publishing
    /// an updated NIP-51 kind 10000 event to relays.
    #[perf_instrument("mute_list")]
    pub async fn block_user(&self, account: &Account, target_pubkey: &PublicKey) -> Result<()> {
        if MuteListEntry::exists(&account.pubkey, target_pubkey, &self.database).await? {
            return Ok(());
        }

        MuteListEntry::insert(&account.pubkey, target_pubkey, true, &self.database).await?;

        if let Err(e) = self.publish_mute_list(account).await {
            tracing::warn!(
                target: "whitenoise::mute_list",
                "Failed to publish mute list after blocking {}: {}",
                target_pubkey,
                e,
            );
        }

        self.emit_block_changed(account, target_pubkey).await;

        Ok(())
    }

    /// Unblocks a user by removing them from the local mute list cache and
    /// publishing an updated NIP-51 kind 10000 event to relays.
    #[perf_instrument("mute_list")]
    pub async fn unblock_user(&self, account: &Account, target_pubkey: &PublicKey) -> Result<()> {
        MuteListEntry::delete(&account.pubkey, target_pubkey, &self.database).await?;

        if let Err(e) = self.publish_mute_list(account).await {
            tracing::warn!(
                target: "whitenoise::mute_list",
                "Failed to publish mute list after unblocking {}: {}",
                target_pubkey,
                e,
            );
        }

        self.emit_block_changed(account, target_pubkey).await;

        Ok(())
    }

    /// Returns all blocked users for the given account.
    #[perf_instrument("mute_list")]
    pub async fn get_blocked_users(&self, account: &Account) -> Result<Vec<MuteListEntry>> {
        let entries = MuteListEntry::find_by_account(&account.pubkey, &self.database).await?;
        Ok(entries)
    }

    /// Returns `true` if the given pubkey is blocked by the account.
    #[perf_instrument("mute_list")]
    pub async fn is_user_blocked(
        &self,
        account_pubkey: &PublicKey,
        target_pubkey: &PublicKey,
    ) -> Result<bool> {
        let blocked = MuteListEntry::exists(account_pubkey, target_pubkey, &self.database).await?;
        Ok(blocked)
    }

    /// Fetches the latest kind 10000 mute list from relays, decrypts the
    /// private section, and replaces the local cache.
    #[perf_instrument("mute_list")]
    pub async fn sync_mute_list(&self, account: &Account) -> Result<()> {
        let signer = self.get_signer_for_account(account)?;
        let relay_urls = self.fallback_relay_urls().await;

        let filter = Filter::new()
            .author(account.pubkey)
            .kind(Kind::MuteList)
            .limit(1);

        let events = self
            .relay_control
            .ephemeral()
            .fetch_events_from(&relay_urls, filter)
            .await?;

        let event = match events.first() {
            Some(event) => event,
            None => {
                tracing::debug!(
                    target: "whitenoise::mute_list",
                    "No mute list event found on relays for {}",
                    account.pubkey,
                );
                return Ok(());
            }
        };

        let mut entries: Vec<(PublicKey, bool)> = Vec::new();

        // Public tags: extract "p" tags
        for tag in event.tags.iter() {
            if let Some(TagStandard::PublicKey { public_key, .. }) = tag.as_standardized() {
                entries.push((*public_key, false));
            }
        }

        // Private content: decrypt with NIP-44 via signer and extract "p" tags.
        // If decryption or parsing fails, abort — do not replace the cache with a partial result.
        if !event.content.is_empty() {
            let decrypted = match signer.nip44_decrypt(&account.pubkey, &event.content).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::mute_list",
                        "Failed to decrypt mute list content: {}",
                        e,
                    );
                    return Ok(());
                }
            };

            let tags = match serde_json::from_str::<Vec<Vec<String>>>(&decrypted) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::mute_list",
                        "Failed to parse private mute list content: {}",
                        e,
                    );
                    return Ok(());
                }
            };

            for tag in &tags {
                if tag.len() >= 2 && tag[0] == "p" && let Ok(pk) = PublicKey::parse(&tag[1]) {
                    entries.push((pk, true));
                }
            }
        }

        MuteListEntry::sync_from_event(&account.pubkey, &entries, &self.database).await?;

        Ok(())
    }

    /// Emits a `UserBlockChanged` chat list update for the DM group with the
    /// target user, if one exists.
    async fn emit_block_changed(&self, account: &Account, target_pubkey: &PublicKey) {
        match AccountGroup::find_dm_group_id_by_peer(&account.pubkey, target_pubkey, &self.database)
            .await
        {
            Ok(Some(group_id)) => {
                self.emit_chat_list_update(
                    account,
                    &group_id,
                    ChatListUpdateTrigger::UserBlockChanged,
                )
                .await;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::mute_list",
                    "Failed to look up DM group for block change emission: {}",
                    e,
                );
            }
        }
    }

    /// Builds and publishes a kind 10000 mute list event from the local cache.
    ///
    /// Private entries are NIP-44 encrypted in the event content.
    /// Public entries are in the event tags.
    #[perf_instrument("mute_list")]
    async fn publish_mute_list(&self, account: &Account) -> Result<()> {
        let signer = self.get_signer_for_account(account)?;
        let relay_urls = self.fallback_relay_urls().await;

        let entries = MuteListEntry::find_by_account(&account.pubkey, &self.database).await?;

        let mut public_tags: Vec<Tag> = Vec::new();
        let mut private_tags: Vec<Vec<String>> = Vec::new();

        for entry in &entries {
            if entry.is_private {
                private_tags.push(vec!["p".to_string(), entry.muted_pubkey.to_hex()]);
            } else {
                public_tags.push(Tag::public_key(entry.muted_pubkey));
            }
        }

        // NIP-44 encrypt the private tags as JSON content via signer
        let content = if private_tags.is_empty() {
            String::new()
        } else {
            let json = serde_json::to_string(&private_tags)
                .map_err(|e| WhitenoiseError::InvalidInput(e.to_string()))?;
            signer
                .nip44_encrypt(&account.pubkey, &json)
                .await
                .map_err(|e| WhitenoiseError::InvalidInput(e.to_string()))?
        };

        let event_builder = EventBuilder::new(Kind::MuteList, content).tags(public_tags);
        let event = event_builder
            .sign(&signer)
            .await
            .map_err(|e| WhitenoiseError::InvalidInput(e.to_string()))?;

        self.relay_control
            .publish_event_to(event, &account.pubkey, &relay_urls)
            .await?;

        Ok(())
    }
}

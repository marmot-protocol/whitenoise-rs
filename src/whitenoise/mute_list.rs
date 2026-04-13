use std::collections::HashSet;

use nostr_sdk::{Event, EventBuilder, Filter, Kind, NostrSigner, PublicKey, Tag, TagStandard};

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
    ///
    /// Fetches the latest mute list from relays first so that blocks made on
    /// other devices are preserved (merge, not replace). The sync is required
    /// — if it fails the operation is aborted so we never publish a stale list
    /// that would silently wipe blocks added on other devices.
    ///
    /// If publishing fails the local insert is rolled back so the caller can
    /// retry the full operation cleanly.
    #[perf_instrument("mute_list")]
    pub async fn block_user(&self, account: &Account, target_pubkey: &PublicKey) -> Result<()> {
        // Guard: a user cannot block themselves.
        if account.pubkey == *target_pubkey {
            return Ok(());
        }

        // Fast path: if already blocked locally no sync or publish is needed.
        if MuteListEntry::exists(&account.pubkey, target_pubkey, &self.database).await? {
            return Ok(());
        }

        // Fail fast — proceeding with a stale local cache after a sync failure
        // would publish {stale + new} and silently wipe remote-only blocks.
        self.sync_mute_list(account).await?;

        // Re-check after sync: another device may have added this block while
        // we were fetching the latest list.
        if MuteListEntry::exists(&account.pubkey, target_pubkey, &self.database).await? {
            return Ok(());
        }

        MuteListEntry::insert(&account.pubkey, target_pubkey, true, &self.database).await?;

        if let Err(e) = self.publish_mute_list(account).await {
            // Roll back the local insert so a retry runs the full flow from scratch.
            if let Err(rb_err) =
                MuteListEntry::delete(&account.pubkey, target_pubkey, &self.database).await
            {
                tracing::warn!(
                    target: "whitenoise::mute_list",
                    "Failed to roll back local block insert for {}: {}",
                    target_pubkey,
                    rb_err,
                );
            }
            return Err(e);
        }

        self.emit_block_changed(account, target_pubkey).await;

        Ok(())
    }

    /// Unblocks a user by removing them from the local mute list cache and
    /// publishing an updated NIP-51 kind 10000 event to relays.
    ///
    /// Fetches the latest mute list from relays first so that blocks made on
    /// other devices are preserved (merge, not replace). The sync is required
    /// — if it fails the operation is aborted so we never publish a stale list
    /// that would silently wipe blocks added on other devices.
    ///
    /// If publishing fails the local delete is rolled back so the caller can
    /// retry the full operation cleanly.
    #[perf_instrument("mute_list")]
    pub async fn unblock_user(&self, account: &Account, target_pubkey: &PublicKey) -> Result<()> {
        if !MuteListEntry::exists(&account.pubkey, target_pubkey, &self.database).await? {
            return Ok(());
        }

        // Fail fast — same data-loss risk as block_user if we proceed with
        // a stale cache after a failed sync.
        self.sync_mute_list(account).await?;

        // Re-check after sync: another device may have already removed this
        // block while we were fetching the latest list.
        if !MuteListEntry::exists(&account.pubkey, target_pubkey, &self.database).await? {
            return Ok(());
        }

        // Capture is_private before deleting so the rollback re-inserts the
        // entry exactly as it was.
        let is_private = MuteListEntry::find_by_account_and_target(
            &account.pubkey,
            target_pubkey,
            &self.database,
        )
        .await?
        .map(|e| e.is_private)
        .unwrap_or(true);

        MuteListEntry::delete(&account.pubkey, target_pubkey, &self.database).await?;

        if let Err(e) = self.publish_mute_list(account).await {
            // Roll back the local delete so a retry runs the full flow from scratch.
            if let Err(rb_err) =
                MuteListEntry::insert(&account.pubkey, target_pubkey, is_private, &self.database)
                    .await
            {
                tracing::warn!(
                    target: "whitenoise::mute_list",
                    "Failed to roll back local unblock delete for {}: {}",
                    target_pubkey,
                    rb_err,
                );
            }
            return Err(e);
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
    pub(crate) async fn sync_mute_list(&self, account: &Account) -> Result<()> {
        let signer = self.get_signer_for_account(account)?;
        let relay_urls = self.account_relay_urls(account).await;

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

        let entries = Self::parse_mute_list_entries(signer.as_ref(), &account.pubkey, event).await;
        let Some(entries) = entries else {
            return Ok(());
        };

        MuteListEntry::sync_from_event(&account.pubkey, &entries, &self.database).await?;

        Ok(())
    }

    /// Parses public and private "p" tags from a kind 10000 mute list event.
    ///
    /// Returns `None` if the event has non-empty content that cannot be decrypted
    /// or parsed — callers must not replace the local cache in that case.
    pub(crate) async fn parse_mute_list_entries(
        signer: &dyn NostrSigner,
        account_pubkey: &PublicKey,
        event: &Event,
    ) -> Option<Vec<(PublicKey, bool)>> {
        let mut entries: Vec<(PublicKey, bool)> = Vec::new();

        // Public tags: extract "p" tags
        for tag in event.tags.iter() {
            if let Some(TagStandard::PublicKey { public_key, .. }) = tag.as_standardized() {
                entries.push((*public_key, false));
            }
        }

        // Private content: decrypt with NIP-44 (fallback NIP-04) and extract "p" tags.
        // If decryption or parsing fails, return None — callers must not replace
        // the local cache with a partial (public-only) result.
        if !event.content.is_empty() {
            let decrypted = match signer.nip44_decrypt(account_pubkey, &event.content).await {
                Ok(d) => d,
                Err(nip44_err) => {
                    tracing::debug!(
                        target: "whitenoise::mute_list",
                        "NIP-44 decrypt failed ({}), trying NIP-04 fallback",
                        nip44_err,
                    );
                    match signer.nip04_decrypt(account_pubkey, &event.content).await {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::warn!(
                                target: "whitenoise::mute_list",
                                "Failed to decrypt mute list content (NIP-44 and NIP-04 both failed): {}",
                                e,
                            );
                            return None;
                        }
                    }
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
                    return None;
                }
            };

            for tag in &tags {
                if tag.len() >= 2
                    && tag[0] == "p"
                    && let Ok(pk) = PublicKey::parse(&tag[1])
                {
                    entries.push((pk, true));
                }
            }
        }

        Some(entries)
    }

    /// Replaces the mute list cache and emits `UserBlockChanged` for every
    /// pubkey that was added or removed compared to the previous state.
    pub(crate) async fn sync_and_emit(
        &self,
        account: &Account,
        entries: &[(PublicKey, bool)],
    ) -> Result<()> {
        let old = MuteListEntry::find_by_account(&account.pubkey, &self.database).await?;
        let old_pubkeys: HashSet<PublicKey> = old.iter().map(|e| e.muted_pubkey).collect();

        MuteListEntry::sync_from_event(&account.pubkey, entries, &self.database).await?;

        let new_pubkeys: HashSet<PublicKey> = entries.iter().map(|(pk, _)| *pk).collect();

        for pubkey in old_pubkeys.symmetric_difference(&new_pubkeys) {
            self.emit_block_changed(account, pubkey).await;
        }

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
        let relay_urls = self.account_relay_urls(account).await;

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

#[cfg(test)]
mod tests {
    use nostr_sdk::{EventBuilder, Keys, Kind, Tag};

    use super::*;
    use crate::whitenoise::database::mute_list::MuteListEntry;
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    #[tokio::test]
    async fn parse_mute_list_entries_public_tags_only() {
        let keys = Keys::generate();
        let signer = keys.clone();
        let target = Keys::generate().public_key();

        let event = EventBuilder::new(Kind::MuteList, "")
            .tags([Tag::public_key(target)])
            .sign(&signer)
            .await
            .unwrap();

        let entries =
            Whitenoise::parse_mute_list_entries(&signer, &keys.public_key(), &event).await;

        let entries = entries.expect("should return Some");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, target);
        assert!(!entries[0].1); // is_private = false
    }

    #[tokio::test]
    async fn parse_mute_list_entries_empty_event_returns_empty_list() {
        let keys = Keys::generate();
        let signer = keys.clone();

        let event = EventBuilder::new(Kind::MuteList, "")
            .sign(&signer)
            .await
            .unwrap();

        let entries =
            Whitenoise::parse_mute_list_entries(&signer, &keys.public_key(), &event).await;

        let entries = entries.expect("should return Some");
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn parse_mute_list_entries_bad_content_returns_none() {
        let keys = Keys::generate();
        let signer = keys.clone();

        // Non-empty content that is not valid NIP-44 ciphertext
        let event = EventBuilder::new(Kind::MuteList, "not-valid-ciphertext")
            .sign(&signer)
            .await
            .unwrap();

        let entries =
            Whitenoise::parse_mute_list_entries(&signer, &keys.public_key(), &event).await;

        // Decrypt failure → None, so cache must not be replaced
        assert!(entries.is_none());
    }

    #[tokio::test]
    async fn block_user_returns_ok_if_already_blocked() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let target = Keys::generate().public_key();

        // Insert directly — bypasses relay so the test is Docker-independent.
        MuteListEntry::insert(&account.pubkey, &target, true, &whitenoise.database)
            .await
            .unwrap();

        // block_user fast-path: exists() check fires before any sync.
        let result = whitenoise.block_user(&account, &target).await;
        assert!(
            result.is_ok(),
            "block_user on already-blocked user should be a no-op"
        );

        // Entry must still be there (not double-inserted or removed).
        assert!(
            MuteListEntry::exists(&account.pubkey, &target, &whitenoise.database)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn unblock_user_returns_ok_if_not_blocked() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let target = Keys::generate().public_key();

        // Target is not blocked — unblock_user must return Ok without touching relays.
        let result = whitenoise.unblock_user(&account, &target).await;
        assert!(
            result.is_ok(),
            "unblock_user on non-blocked user should be a no-op"
        );
    }

    // ── get_blocked_users / is_user_blocked (pure DB, no relay) ─────────────

    #[tokio::test]
    async fn get_blocked_users_returns_all_blocked_for_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let target1 = Keys::generate().public_key();
        let target2 = Keys::generate().public_key();

        MuteListEntry::insert(&account.pubkey, &target1, true, &whitenoise.database)
            .await
            .unwrap();
        MuteListEntry::insert(&account.pubkey, &target2, false, &whitenoise.database)
            .await
            .unwrap();

        let blocked = whitenoise.get_blocked_users(&account).await.unwrap();
        assert_eq!(blocked.len(), 2);

        let pubkeys: Vec<_> = blocked.iter().map(|e| e.muted_pubkey).collect();
        assert!(pubkeys.contains(&target1));
        assert!(pubkeys.contains(&target2));
    }

    #[tokio::test]
    async fn is_user_blocked_returns_true_when_blocked() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let blocked_target = Keys::generate().public_key();
        let other_target = Keys::generate().public_key();

        MuteListEntry::insert(&account.pubkey, &blocked_target, true, &whitenoise.database)
            .await
            .unwrap();

        assert!(
            whitenoise
                .is_user_blocked(&account.pubkey, &blocked_target)
                .await
                .unwrap(),
            "inserted target should be reported as blocked"
        );
        assert!(
            !whitenoise
                .is_user_blocked(&account.pubkey, &other_target)
                .await
                .unwrap(),
            "uninserted target should not be reported as blocked"
        );
    }

    #[tokio::test]
    async fn get_blocked_users_returns_empty_for_account_with_no_blocks() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let blocked = whitenoise.get_blocked_users(&account).await.unwrap();
        assert!(blocked.is_empty());
    }

    // ── parse_mute_list_entries: private tags ────────────────────────────────

    #[tokio::test]
    async fn parse_mute_list_entries_private_tags_decrypted() {
        let keys = Keys::generate();
        let signer = keys.clone();
        let target = Keys::generate().public_key();

        // Build the encrypted private content: [[\"p\", \"<hex>\"]]
        let private_json =
            serde_json::to_string(&vec![vec!["p".to_string(), target.to_hex()]]).unwrap();
        let encrypted = signer
            .nip44_encrypt(&keys.public_key(), &private_json)
            .await
            .unwrap();

        let event = EventBuilder::new(Kind::MuteList, encrypted)
            .sign(&signer)
            .await
            .unwrap();

        let entries =
            Whitenoise::parse_mute_list_entries(&signer, &keys.public_key(), &event).await;

        let entries = entries.expect("should return Some");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, target);
        assert!(entries[0].1); // is_private = true
    }

    #[tokio::test]
    async fn parse_mute_list_entries_private_and_public_combined() {
        let keys = Keys::generate();
        let signer = keys.clone();
        let public_target = Keys::generate().public_key();
        let private_target = Keys::generate().public_key();

        let private_json =
            serde_json::to_string(&vec![vec!["p".to_string(), private_target.to_hex()]]).unwrap();
        let encrypted = signer
            .nip44_encrypt(&keys.public_key(), &private_json)
            .await
            .unwrap();

        let event = EventBuilder::new(Kind::MuteList, encrypted)
            .tags([Tag::public_key(public_target)])
            .sign(&signer)
            .await
            .unwrap();

        let entries =
            Whitenoise::parse_mute_list_entries(&signer, &keys.public_key(), &event).await;

        let entries = entries.expect("should return Some");
        assert_eq!(entries.len(), 2);

        let public_entry = entries.iter().find(|(pk, _)| *pk == public_target);
        let private_entry = entries.iter().find(|(pk, _)| *pk == private_target);
        assert!(public_entry.is_some() && !public_entry.unwrap().1);
        assert!(private_entry.is_some() && private_entry.unwrap().1);
    }

    #[tokio::test]
    async fn parse_mute_list_entries_invalid_json_content_returns_none() {
        let keys = Keys::generate();
        let signer = keys.clone();

        // Encrypt valid ciphertext but the decrypted content is not valid tag JSON
        let invalid_json = "not-a-json-array";
        let encrypted = signer
            .nip44_encrypt(&keys.public_key(), invalid_json)
            .await
            .unwrap();

        let event = EventBuilder::new(Kind::MuteList, encrypted)
            .sign(&signer)
            .await
            .unwrap();

        let entries =
            Whitenoise::parse_mute_list_entries(&signer, &keys.public_key(), &event).await;

        // JSON parse failure → None
        assert!(entries.is_none());
    }

    // ── sync_and_emit ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn sync_and_emit_updates_cache_from_empty() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let target1 = Keys::generate().public_key();
        let target2 = Keys::generate().public_key();

        let entries = vec![(target1, true), (target2, false)];
        whitenoise.sync_and_emit(&account, &entries).await.unwrap();

        let blocked = whitenoise.get_blocked_users(&account).await.unwrap();
        assert_eq!(blocked.len(), 2);
        assert!(
            MuteListEntry::exists(&account.pubkey, &target1, &whitenoise.database)
                .await
                .unwrap()
        );
        assert!(
            MuteListEntry::exists(&account.pubkey, &target2, &whitenoise.database)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn sync_and_emit_replaces_old_entries() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let old_target = Keys::generate().public_key();
        let new_target = Keys::generate().public_key();

        // Seed with old_target
        MuteListEntry::insert(&account.pubkey, &old_target, true, &whitenoise.database)
            .await
            .unwrap();

        // Sync with only new_target
        whitenoise
            .sync_and_emit(&account, &[(new_target, false)])
            .await
            .unwrap();

        assert!(
            !MuteListEntry::exists(&account.pubkey, &old_target, &whitenoise.database)
                .await
                .unwrap(),
            "old_target should be removed"
        );
        assert!(
            MuteListEntry::exists(&account.pubkey, &new_target, &whitenoise.database)
                .await
                .unwrap(),
            "new_target should be present"
        );
    }

    #[tokio::test]
    async fn sync_and_emit_with_empty_entries_clears_cache() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let target = Keys::generate().public_key();

        MuteListEntry::insert(&account.pubkey, &target, true, &whitenoise.database)
            .await
            .unwrap();

        whitenoise.sync_and_emit(&account, &[]).await.unwrap();

        let blocked = whitenoise.get_blocked_users(&account).await.unwrap();
        assert!(blocked.is_empty());
    }

    // ── emit_block_changed: no DM group ──────────────────────────────────────

    #[tokio::test]
    async fn emit_block_changed_noop_when_no_dm_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let stranger = Keys::generate().public_key();

        // No DM group exists for this pair — emit_block_changed must not panic.
        // We verify indirectly: block_user path calls it only after publish,
        // so we insert directly and call sync_and_emit which internally calls it.
        let entries = vec![(stranger, true)];
        let result = whitenoise.sync_and_emit(&account, &entries).await;
        assert!(
            result.is_ok(),
            "sync_and_emit with no DM group must succeed"
        );
    }
}

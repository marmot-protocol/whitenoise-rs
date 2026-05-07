//! Mute list (block/unblock) operations scoped to a single account session.
//!
//! `MuteListOps` is a borrow-based view that provides NIP-51 mute list
//! management — blocking, unblocking, querying blocked users, syncing from
//! relays, and emitting chat list updates — all without threading
//! `account_pubkey` through every call.

use std::collections::HashSet;

use nostr_sdk::{Event, EventBuilder, Filter, Kind, NostrSigner, PublicKey, Tag, TagStandard};

use super::AccountSession;
use crate::perf_instrument;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::accounts_groups::AccountGroup;
use crate::whitenoise::chat_list_streaming::ChatListUpdateTrigger;
use crate::whitenoise::database::mute_list::MuteListEntry;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::relays::{Relay, RelayType};

/// Account-scoped mute list operations.
pub struct MuteListOps<'a> {
    session: &'a AccountSession,
}

impl<'a> MuteListOps<'a> {
    pub(super) fn new(session: &'a AccountSession) -> Self {
        Self { session }
    }

    // ── Public API ────────────────────────────────────────────────────

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
    pub async fn block_user(&self, target_pubkey: &PublicKey) -> Result<()> {
        let account_pubkey = &self.session.account_pubkey;

        // Guard: a user cannot block themselves.
        if *account_pubkey == *target_pubkey {
            return Ok(());
        }

        // Fast path: if already blocked locally no sync or publish is needed.
        if MuteListEntry::exists(target_pubkey, self.db()).await? {
            return Ok(());
        }

        // Fail fast — proceeding with a stale local cache after a sync failure
        // would publish {stale + new} and silently wipe remote-only blocks.
        self.sync_mute_list().await?;

        // Re-check after sync: another device may have added this block while
        // we were fetching the latest list.
        if MuteListEntry::exists(target_pubkey, self.db()).await? {
            return Ok(());
        }

        MuteListEntry::insert(target_pubkey, true, self.db()).await?;

        if let Err(e) = self.publish_mute_list().await {
            // Roll back the local insert so a retry runs the full flow from scratch.
            if let Err(rb_err) = MuteListEntry::delete(target_pubkey, self.db()).await {
                tracing::warn!(
                    target: "whitenoise::mute_list",
                    "Failed to roll back local block insert for {}: {}",
                    target_pubkey,
                    rb_err,
                );
            }
            return Err(e);
        }

        self.emit_block_changed(target_pubkey).await;

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
    pub async fn unblock_user(&self, target_pubkey: &PublicKey) -> Result<()> {
        if !MuteListEntry::exists(target_pubkey, self.db()).await? {
            return Ok(());
        }

        // Fail fast — same data-loss risk as block_user if we proceed with
        // a stale cache after a failed sync.
        self.sync_mute_list().await?;

        // Re-check after sync: another device may have already removed this
        // block while we were fetching the latest list.
        if !MuteListEntry::exists(target_pubkey, self.db()).await? {
            return Ok(());
        }

        // Capture is_private before deleting so the rollback re-inserts the
        // entry exactly as it was. If the entry vanished between the
        // exists-check above and now (concurrent sync from another flow),
        // default to private — the safer choice for a rollback.
        let is_private = match MuteListEntry::find_by_muted_pubkey(target_pubkey, self.db()).await?
        {
            Some(e) => e.is_private,
            None => {
                tracing::debug!(
                    target: "whitenoise::mute_list",
                    "Mute entry for {} vanished between exists-check and read; \
                     defaulting is_private=true for rollback",
                    target_pubkey,
                );
                true
            }
        };

        MuteListEntry::delete(target_pubkey, self.db()).await?;

        if let Err(e) = self.publish_mute_list().await {
            // Roll back the local delete so a retry runs the full flow from scratch.
            if let Err(rb_err) = MuteListEntry::insert(target_pubkey, is_private, self.db()).await {
                tracing::warn!(
                    target: "whitenoise::mute_list",
                    "Failed to roll back local unblock delete for {}: {}",
                    target_pubkey,
                    rb_err,
                );
            }
            return Err(e);
        }

        self.emit_block_changed(target_pubkey).await;

        Ok(())
    }

    /// Returns all blocked users for this account.
    #[perf_instrument("mute_list")]
    pub async fn get_blocked_users(&self) -> Result<Vec<MuteListEntry>> {
        let entries = MuteListEntry::find_all(self.db()).await?;
        Ok(entries)
    }

    /// Returns `true` if the given pubkey is blocked by this account.
    #[perf_instrument("mute_list")]
    pub async fn is_user_blocked(&self, target_pubkey: &PublicKey) -> Result<bool> {
        let blocked = MuteListEntry::exists(target_pubkey, self.db()).await?;
        Ok(blocked)
    }

    /// Replaces the mute list cache and emits `UserBlockChanged` for every
    /// pubkey that was added or removed compared to the previous state.
    pub(crate) async fn sync_and_emit(&self, entries: &[(PublicKey, bool)]) -> Result<()> {
        let old = MuteListEntry::find_all(self.db()).await?;
        let old_pubkeys: HashSet<PublicKey> = old.iter().map(|e| e.muted_pubkey).collect();

        MuteListEntry::sync_from_event(entries, self.db()).await?;

        let new_pubkeys: HashSet<PublicKey> = entries.iter().map(|(pk, _)| *pk).collect();

        for pubkey in old_pubkeys.symmetric_difference(&new_pubkeys) {
            self.emit_block_changed(pubkey).await;
        }

        Ok(())
    }

    // ── Internal helpers ──────────────────────────────────────────────

    /// Fetches the latest kind 10000 mute list from relays, decrypts the
    /// private section, and replaces the local cache.
    #[perf_instrument("mute_list")]
    async fn sync_mute_list(&self) -> Result<()> {
        let signer = self.require_signer().await?;
        let relay_urls = self.account_relay_urls().await;

        let filter = Filter::new()
            .author(self.session.account_pubkey)
            .kind(Kind::MuteList)
            .limit(1);

        let events = self
            .session
            .shared
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
                    self.session.account_pubkey,
                );
                return Ok(());
            }
        };

        let entries =
            Self::parse_mute_list_entries(signer.as_ref(), &self.session.account_pubkey, event)
                .await;
        let Some(entries) = entries else {
            return Ok(());
        };

        MuteListEntry::sync_from_event(&entries, self.db()).await?;

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

    /// Builds and publishes a kind 10000 mute list event from the local cache.
    ///
    /// Private entries are NIP-44 encrypted in the event content.
    /// Public entries are in the event tags.
    #[perf_instrument("mute_list")]
    async fn publish_mute_list(&self) -> Result<()> {
        let signer = self.require_signer().await?;
        let relay_urls = self.account_relay_urls().await;

        let entries = MuteListEntry::find_all(self.db()).await?;

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
                .nip44_encrypt(&self.session.account_pubkey, &json)
                .await
                .map_err(|e| WhitenoiseError::InvalidInput(e.to_string()))?
        };

        let event_builder = EventBuilder::new(Kind::MuteList, content).tags(public_tags);
        let event = event_builder
            .sign(&signer)
            .await
            .map_err(|e| WhitenoiseError::InvalidInput(e.to_string()))?;

        self.session
            .shared
            .relay_control
            .publish_event_to(event, &self.session.account_pubkey, &relay_urls)
            .await?;

        Ok(())
    }

    /// Emits a `UserBlockChanged` chat list update for the DM group with the
    /// target user, if one exists.
    async fn emit_block_changed(&self, target_pubkey: &PublicKey) {
        match AccountGroup::find_dm_group_id_by_peer(
            target_pubkey,
            &self.session.account_db.inner.pool,
        )
        .await
        {
            Ok(Some(group_id)) => {
                self.session
                    .chat_list()
                    .emit_update(&group_id, ChatListUpdateTrigger::UserBlockChanged)
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

    /// Read the current signer, returning an error if none is set.
    async fn require_signer(&self) -> Result<std::sync::Arc<dyn NostrSigner>> {
        self.session
            .signer
            .read()
            .await
            .clone()
            .ok_or(WhitenoiseError::SignerUnavailable(
                self.session.account_pubkey,
            ))
    }

    /// Returns relay URLs for this account's NIP-65 relay list,
    /// falling back to discovery plane relays.
    async fn account_relay_urls(&self) -> Vec<nostr_sdk::RelayUrl> {
        let shared = &self.session.shared.database;
        let nip65 = async {
            let acct = Account::find_by_pubkey(&self.session.account_pubkey, shared)
                .await
                .ok()?;
            let user = acct.user(shared).await.ok()?;
            let relays = user.relays(RelayType::Nip65, shared).await.ok()?;
            Some(relays)
        }
        .await
        .unwrap_or_default();

        if !nip65.is_empty() {
            Relay::urls(&nip65)
        } else {
            self.session
                .shared
                .relay_control
                .discovery()
                .relays()
                .to_vec()
        }
    }

    fn db(&self) -> &crate::whitenoise::database::account_db::AccountDatabase {
        &self.session.account_db
    }
}

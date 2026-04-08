//! Push notification registration state and per-group token cache models.

use core::fmt;
use std::sync::Arc;
use std::time::Duration;
use std::{
    collections::{BTreeMap, HashSet},
    str::FromStr,
};

use ::rand::Rng;
use chrono::{DateTime, Utc};
use mdk_core::mip05::{
    EncryptedToken, LeafTokenTag, Mip05GroupMessage, NotificationPlatform, PushTokenPlaintext,
    TokenTag, build_notification_batches, build_token_list_response_rumor,
    build_token_removal_rumor, build_token_request_rumor, encrypt_push_token, parse_group_message,
};
use mdk_core::prelude::{GroupId, group_types::GroupState};
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr_sdk::{EventId, Kind, PublicKey, RelayUrl};
use serde::{Deserialize, Serialize};

use crate::whitenoise::{
    Whitenoise, WhitenoiseConfig,
    account_settings::AccountSettings,
    accounts::Account,
    accounts_groups::AccountGroup,
    database::Database,
    error::{Result, WhitenoiseError},
    relays::{Relay, RelayType},
    users::UserRelaySyncContext,
};
use crate::{
    perf_instrument,
    relay_control::{RelayControlPlane, ephemeral::EphemeralPlane},
};

/// Supported native push-token platforms for device registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PushPlatform {
    #[serde(rename = "apns")]
    Apns,
    #[serde(rename = "fcm")]
    Fcm,
}

impl PushPlatform {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Apns => "apns",
            Self::Fcm => "fcm",
        }
    }
}

impl fmt::Display for PushPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PushPlatform {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "apns" => Ok(Self::Apns),
            "fcm" => Ok(Self::Fcm),
            _ => Err(format!("Invalid push platform: {value}")),
        }
    }
}

/// This device's locally persisted push registration for a specific account.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PushRegistration {
    pub account_pubkey: PublicKey,
    pub platform: PushPlatform,
    pub raw_token: String,
    pub server_pubkey: PublicKey,
    pub relay_hint: Option<RelayUrl>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_shared_at: Option<DateTime<Utc>>,
}

/// Cached encrypted push token for a group member leaf.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GroupPushToken {
    pub account_pubkey: PublicKey,
    pub mls_group_id: GroupId,
    pub member_pubkey: PublicKey,
    pub leaf_index: u32,
    pub server_pubkey: PublicKey,
    pub relay_hint: Option<RelayUrl>,
    pub encrypted_token: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl fmt::Debug for PushRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PushRegistration")
            .field("account_pubkey", &self.account_pubkey)
            .field("platform", &self.platform)
            .field("raw_token", &"<redacted>")
            .field("server_pubkey", &self.server_pubkey)
            .field("relay_hint", &self.relay_hint)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .field("last_shared_at", &self.last_shared_at)
            .finish()
    }
}

impl fmt::Debug for GroupPushToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GroupPushToken")
            .field("account_pubkey", &self.account_pubkey)
            .field("mls_group_id", &self.mls_group_id)
            .field("member_pubkey", &self.member_pubkey)
            .field("leaf_index", &self.leaf_index)
            .field("server_pubkey", &self.server_pubkey)
            .field("relay_hint", &self.relay_hint)
            .field("encrypted_token", &"<redacted>")
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

const PUSH_GROUP_MESSAGE_KINDS: [u16; 3] = [
    mdk_core::mip05::TOKEN_REQUEST_KIND,
    mdk_core::mip05::TOKEN_LIST_RESPONSE_KIND,
    mdk_core::mip05::TOKEN_REMOVAL_KIND,
];

#[derive(Clone)]
struct PendingTokenResponseContext {
    config: WhitenoiseConfig,
    database: Arc<Database>,
    pending_push_token_responses: Arc<dashmap::DashMap<(PublicKey, GroupId, EventId), ()>>,
    relay_control: Arc<RelayControlPlane>,
}

pub(crate) fn is_push_group_message_kind(kind: Kind) -> bool {
    PUSH_GROUP_MESSAGE_KINDS.contains(&kind.as_u16())
}

impl PendingTokenResponseContext {
    fn clear_pending_token_response(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        request_event_id: &EventId,
    ) -> bool {
        self.pending_push_token_responses
            .remove(&(*account_pubkey, group_id.clone(), *request_event_id))
            .is_some()
    }

    async fn dispatch_pending_token_response(
        &self,
        account: &Account,
        group_id: &GroupId,
        request_event_id: EventId,
    ) -> Result<bool> {
        if !self.clear_pending_token_response(&account.pubkey, group_id, &request_event_id) {
            return Ok(false);
        }

        self.respond_to_token_request(account, group_id, request_event_id)
            .await?;
        Ok(true)
    }

    async fn respond_to_token_request(
        &self,
        account: &Account,
        group_id: &GroupId,
        request_event_id: EventId,
    ) -> Result<()> {
        let mdk = Account::create_mdk(
            account.pubkey,
            &self.config.data_dir,
            &self.config.keyring_service_id,
        )?;
        let token_tags = group_push_token_tags_for_response_with(
            &account.pubkey,
            group_id,
            &self.database,
            &mdk,
        )
        .await?;

        if token_tags.is_empty() {
            return Ok(());
        }

        let rumor = build_token_list_response_rumor(
            account.pubkey,
            nostr_sdk::Timestamp::now(),
            request_event_id,
            token_tags,
        )?;
        publish_push_group_message_with(&self.config, &self.relay_control, account, group_id, rumor)
            .await
    }
}

async fn group_push_token_tags_for_response_with(
    account_pubkey: &PublicKey,
    group_id: &GroupId,
    database: &Database,
    mdk: &mdk_core::prelude::MDK<MdkSqliteStorage>,
) -> Result<Vec<LeafTokenTag>> {
    let tokens =
        GroupPushToken::find_by_account_and_group(account_pubkey, group_id, database).await?;
    let active_leaf_map = mdk.group_leaf_map(group_id)?;

    let mut response_tokens = Vec::with_capacity(tokens.len());
    for token in tokens {
        let Some(active_member_pubkey) = active_leaf_map.get(&token.leaf_index) else {
            tracing::warn!(
                target: "whitenoise::push_notifications",
                account = %account_pubkey.to_hex(),
                group_id = %hex::encode(group_id.as_slice()),
                member_pubkey = %token.member_pubkey.to_hex(),
                leaf_index = token.leaf_index,
                "Skipping cached push token for inactive group leaf"
            );
            continue;
        };

        if active_member_pubkey != &token.member_pubkey {
            tracing::warn!(
                target: "whitenoise::push_notifications",
                account = %account_pubkey.to_hex(),
                group_id = %hex::encode(group_id.as_slice()),
                member_pubkey = %token.member_pubkey.to_hex(),
                leaf_index = token.leaf_index,
                active_member_pubkey = %active_member_pubkey.to_hex(),
                "Skipping cached push token whose member pubkey no longer matches the active leaf"
            );
            continue;
        }

        let Some(relay_hint) = token.relay_hint.clone() else {
            tracing::warn!(
                target: "whitenoise::push_notifications",
                account = %account_pubkey.to_hex(),
                group_id = %hex::encode(group_id.as_slice()),
                member_pubkey = %token.member_pubkey.to_hex(),
                "Skipping cached push token without relay hint in token-list response"
            );
            continue;
        };
        let encrypted_token = match EncryptedToken::from_base64(&token.encrypted_token) {
            Ok(encrypted_token) => encrypted_token,
            Err(error) => {
                tracing::warn!(
                    target: "whitenoise::push_notifications",
                    account = %account_pubkey.to_hex(),
                    group_id = %hex::encode(group_id.as_slice()),
                    member_pubkey = %token.member_pubkey.to_hex(),
                    error = %error,
                    "Skipping cached push token with invalid encrypted payload in token-list response"
                );
                continue;
            }
        };

        response_tokens.push(LeafTokenTag {
            leaf_index: token.leaf_index,
            token_tag: TokenTag {
                encrypted_token,
                server_pubkey: token.server_pubkey,
                relay_hint,
            },
        });
    }

    Ok(response_tokens)
}

async fn publish_push_group_message_with(
    config: &WhitenoiseConfig,
    relay_control: &RelayControlPlane,
    account: &Account,
    group_id: &GroupId,
    rumor: nostr_sdk::UnsignedEvent,
) -> Result<()> {
    let mdk = Account::create_mdk(account.pubkey, &config.data_dir, &config.keyring_service_id)?;
    let relay_urls = Whitenoise::ensure_group_relays(&mdk, group_id)?;
    let event = mdk.create_message(group_id, rumor, None)?;

    relay_control
        .publish_event_to(event, &account.pubkey, &relay_urls)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotificationRelaySource {
    Cached,
    Synced,
    HintFallback,
}

impl NotificationRelaySource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Cached => "cached",
            Self::Synced => "synced",
            Self::HintFallback => "hint-fallback",
        }
    }
}

fn dedupe_relay_urls(relay_urls: &[RelayUrl]) -> Vec<RelayUrl> {
    let mut deduped = Vec::with_capacity(relay_urls.len());
    let mut seen = HashSet::with_capacity(relay_urls.len());

    for relay_url in relay_urls {
        if seen.insert(relay_url.clone()) {
            deduped.push(relay_url.clone());
        }
    }

    deduped
}

fn prepare_notification_recipient_tokens(
    account_pubkey: &PublicKey,
    cached_tokens: &[GroupPushToken],
    active_leaf_map: &BTreeMap<u32, PublicKey>,
    sender_member_pubkey: &PublicKey,
) -> Vec<TokenTag> {
    let mut recipient_tokens = Vec::with_capacity(cached_tokens.len());
    let mut seen_tokens: HashSet<(PublicKey, EncryptedToken)> =
        HashSet::with_capacity(cached_tokens.len());
    let mut skipped_inactive_leaf = 0usize;
    let mut skipped_member_mismatch = 0usize;
    let mut skipped_missing_hint = 0usize;
    let mut skipped_invalid_encrypted_payload = 0usize;
    let mut skipped_duplicate_token = 0usize;

    for token in cached_tokens {
        let Some(active_member_pubkey) = active_leaf_map.get(&token.leaf_index) else {
            skipped_inactive_leaf += 1;
            continue;
        };

        if active_member_pubkey != &token.member_pubkey {
            skipped_member_mismatch += 1;
            continue;
        }

        if active_member_pubkey == sender_member_pubkey {
            continue;
        }

        let Some(relay_hint) = token.relay_hint.clone() else {
            skipped_missing_hint += 1;
            continue;
        };

        let encrypted_token = match EncryptedToken::from_base64(&token.encrypted_token) {
            Ok(encrypted_token) => encrypted_token,
            Err(_error) => {
                skipped_invalid_encrypted_payload += 1;
                continue;
            }
        };

        if !seen_tokens.insert((token.server_pubkey, encrypted_token.clone())) {
            skipped_duplicate_token += 1;
            continue;
        }

        recipient_tokens.push(TokenTag {
            encrypted_token,
            server_pubkey: token.server_pubkey,
            relay_hint,
        });
    }

    let skipped_total = skipped_inactive_leaf
        + skipped_member_mismatch
        + skipped_missing_hint
        + skipped_invalid_encrypted_payload
        + skipped_duplicate_token;
    if skipped_total > 0 {
        tracing::warn!(
            target: "whitenoise::push_notifications",
            account = %account_pubkey.to_hex(),
            cached_token_count = cached_tokens.len(),
            prepared_token_count = recipient_tokens.len(),
            skipped_total,
            skipped_inactive_leaf,
            skipped_member_mismatch,
            skipped_missing_hint,
            skipped_invalid_encrypted_payload,
            skipped_duplicate_token,
            "Skipped cached push tokens during notification publish"
        );
    }

    recipient_tokens
}

/// Returns per-server token counts for logging/observability only.
fn notification_token_counts_by_server(tokens: &[TokenTag]) -> BTreeMap<PublicKey, usize> {
    let mut counts = BTreeMap::new();

    for token in tokens {
        *counts.entry(token.server_pubkey).or_insert(0) += 1;
    }

    counts
}

async fn resolve_notification_server_relays(
    database: &Database,
    user_relay_sync: &UserRelaySyncContext,
    server_pubkey: PublicKey,
    relay_hints: &[RelayUrl],
) -> Result<(Vec<RelayUrl>, NotificationRelaySource)> {
    let (server_user, _created) =
        crate::whitenoise::users::User::find_or_create_by_pubkey(&server_pubkey, database).await?;
    let deduped_hints = dedupe_relay_urls(relay_hints);

    let cached_relays = server_user.relays(RelayType::Inbox, database).await?;
    if !cached_relays.is_empty() {
        return Ok((
            dedupe_relay_urls(&Relay::urls(&cached_relays)),
            NotificationRelaySource::Cached,
        ));
    }

    if deduped_hints.is_empty() {
        return Ok((Vec::new(), NotificationRelaySource::HintFallback));
    }

    let synced_relays = match user_relay_sync
        .sync_relay_type_for_pubkey(server_pubkey, RelayType::Inbox, &deduped_hints)
        .await
    {
        Ok(relays) => relays,
        Err(error) => {
            tracing::warn!(
                target: "whitenoise::push_notifications",
                server_pubkey = %server_pubkey.to_hex(),
                relay_source = NotificationRelaySource::Synced.as_str(),
                relay_hint_count = deduped_hints.len(),
                error = %error,
                "Failed to sync notification server inbox relays from relay hints"
            );
            server_user.relays(RelayType::Inbox, database).await?
        }
    };
    if !synced_relays.is_empty() {
        return Ok((
            dedupe_relay_urls(&Relay::urls(&synced_relays)),
            NotificationRelaySource::Synced,
        ));
    }

    Ok((deduped_hints, NotificationRelaySource::HintFallback))
}

async fn publish_notification_batches_best_effort(
    database: &Database,
    user_relay_sync: &UserRelaySyncContext,
    ephemeral: &EphemeralPlane,
    account_pubkey: PublicKey,
    token_counts_by_server: &BTreeMap<PublicKey, usize>,
    batches: Vec<mdk_core::prelude::NotificationEventBatch>,
) {
    for batch in batches {
        let token_count = token_counts_by_server
            .get(&batch.server_pubkey)
            .copied()
            .unwrap_or_default();
        let event_count = batch.events.len();
        let relay_resolution = resolve_notification_server_relays(
            database,
            user_relay_sync,
            batch.server_pubkey,
            &batch.relay_hints,
        )
        .await;

        let (relay_urls, relay_source) = match relay_resolution {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(
                    target: "whitenoise::push_notifications",
                    server_pubkey = %batch.server_pubkey.to_hex(),
                    token_count,
                    publish_outcome = "relay-resolution-failed",
                    error = %error,
                    "Failed to resolve notification server relays"
                );
                continue;
            }
        };

        if relay_urls.is_empty() {
            tracing::warn!(
                target: "whitenoise::push_notifications",
                server_pubkey = %batch.server_pubkey.to_hex(),
                token_count,
                relay_source = relay_source.as_str(),
                publish_outcome = "skipped-no-relays",
                "Skipping notification request publish because no server relays were available"
            );
            continue;
        }

        for (event_index, event) in batch.events.into_iter().enumerate() {
            match ephemeral
                .publish_event_to(event, &account_pubkey, &relay_urls)
                .await
            {
                Ok(output) => {
                    tracing::info!(
                        target: "whitenoise::push_notifications",
                        server_pubkey = %batch.server_pubkey.to_hex(),
                        token_count,
                        relay_source = relay_source.as_str(),
                        accepted_relays = output.success.len(),
                        failed_relays = output.failed.len(),
                        event_index = event_index + 1,
                        event_count,
                        publish_outcome = "accepted",
                        "Published best-effort notification request batch"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        target: "whitenoise::push_notifications",
                        server_pubkey = %batch.server_pubkey.to_hex(),
                        token_count,
                        relay_source = relay_source.as_str(),
                        event_index = event_index + 1,
                        event_count,
                        publish_outcome = "failed",
                        error = %error,
                        "Failed to publish best-effort notification request batch"
                    );
                }
            }
        }
    }
}

pub(crate) async fn publish_notification_requests_after_delivery_with(
    config: &WhitenoiseConfig,
    database: &Database,
    user_relay_sync: &UserRelaySyncContext,
    ephemeral: &EphemeralPlane,
    account_pubkey: PublicKey,
    group_id: &GroupId,
) -> Result<()> {
    let mdk = Account::create_mdk(account_pubkey, &config.data_dir, &config.keyring_service_id)?;
    let own_leaf_index = mdk.own_leaf_index(group_id)?;
    let active_leaf_map = mdk.group_leaf_map(group_id)?;
    let sender_member_pubkey = active_leaf_map
        .get(&own_leaf_index)
        .copied()
        .ok_or_else(|| {
            WhitenoiseError::Configuration(
                "sender leaf index missing from active group leaf map".to_string(),
            )
        })?;
    let cached_tokens =
        GroupPushToken::find_by_account_and_group(&account_pubkey, group_id, database).await?;
    let recipient_tokens = prepare_notification_recipient_tokens(
        &account_pubkey,
        &cached_tokens,
        &active_leaf_map,
        &sender_member_pubkey,
    );

    if recipient_tokens.is_empty() {
        return Ok(());
    }

    let token_counts_by_server = notification_token_counts_by_server(&recipient_tokens);
    let batches = build_notification_batches(recipient_tokens)?;

    publish_notification_batches_best_effort(
        database,
        user_relay_sync,
        ephemeral,
        account_pubkey,
        &token_counts_by_server,
        batches,
    )
    .await;

    Ok(())
}

impl Whitenoise {
    /// Returns the locally stored push registration for `account`, if present.
    #[perf_instrument("push_notifications")]
    pub async fn push_registration(&self, account: &Account) -> Result<Option<PushRegistration>> {
        Ok(PushRegistration::find_by_account_pubkey(&account.pubkey, &self.database).await?)
    }

    /// Creates or replaces the locally stored push registration for `account`.
    ///
    /// This updates local persistence and, when notifications remain shareable,
    /// best-effort MIP-05 token sharing for the account's joined groups.
    #[perf_instrument("push_notifications")]
    pub async fn upsert_push_registration(
        &self,
        account: &Account,
        platform: PushPlatform,
        raw_token: &str,
        server_pubkey: &PublicKey,
        relay_hint: Option<&RelayUrl>,
    ) -> Result<PushRegistration> {
        validate_raw_token(raw_token)?;
        let pending_registration = PushRegistration {
            account_pubkey: account.pubkey,
            platform,
            raw_token: raw_token.to_string(),
            server_pubkey: *server_pubkey,
            relay_hint: relay_hint.cloned(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_shared_at: None,
        };
        pending_registration.push_token_plaintext()?;

        let previous_token_tag = self
            .push_registration(account)
            .await?
            .as_ref()
            .map(PushRegistration::token_tag)
            .transpose()?
            .flatten();
        let new_token_tag = pending_registration.token_tag()?;

        let registration = PushRegistration::upsert(
            &account.pubkey,
            platform,
            raw_token,
            server_pubkey,
            relay_hint,
            &self.database,
        )
        .await?;

        let notifications_enabled =
            AccountSettings::notifications_enabled_for_pubkey(&account.pubkey, &self.database)
                .await?;

        if previous_token_tag.is_some() && new_token_tag.is_none() {
            if let Err(error) = self
                .remove_local_push_token_from_joined_groups(account)
                .await
            {
                tracing::warn!(
                    target: "whitenoise::push_notifications",
                    account = %account.pubkey.to_hex(),
                    error = %error,
                    "Failed to remove previously shared push token after registration became unshareable"
                );
            }
        } else if notifications_enabled
            && let Some(new_token_tag) = new_token_tag.as_ref()
            && let Err(error) = self
                .share_push_token_to_joined_groups(account, new_token_tag)
                .await
        {
            tracing::warn!(
                target: "whitenoise::push_notifications",
                account = %account.pubkey.to_hex(),
                error = %error,
                "Failed to share updated push registration to joined groups"
            );
        }

        Ok(registration)
    }

    /// Removes the locally stored push registration for `account`.
    #[perf_instrument("push_notifications")]
    pub async fn clear_push_registration(&self, account: &Account) -> Result<()> {
        PushRegistration::delete_by_account_pubkey(&account.pubkey, &self.database).await?;

        if let Err(error) = self
            .remove_local_push_token_from_joined_groups(account)
            .await
        {
            tracing::warn!(
                target: "whitenoise::push_notifications",
                account = %account.pubkey.to_hex(),
                error = %error,
                "Failed to remove shared push token from joined groups after local clear"
            );
        }

        Ok(())
    }

    #[perf_instrument("push_notifications")]
    pub(crate) async fn handle_received_push_group_message(
        &self,
        account: &Account,
        message: &mdk_core::prelude::message_types::Message,
        sender_leaf_index: Option<u32>,
    ) -> Result<bool> {
        if !is_push_group_message_kind(message.kind) {
            return Ok(false);
        }

        let group_message = parse_group_message(message)?;

        match group_message {
            Mip05GroupMessage::TokenRequest(request) => {
                let leaf_index = sender_leaf_index.ok_or_else(|| {
                    WhitenoiseError::InvalidEvent(
                        "MIP-05 token request missing sender leaf index".to_string(),
                    )
                })?;

                self.merge_token_request(
                    account,
                    &message.mls_group_id,
                    message.event.pubkey,
                    leaf_index,
                    request,
                )
                .await?;

                if let Some(request_event_id) = message.event.id {
                    self.schedule_pending_token_response(
                        account.clone(),
                        message.mls_group_id.clone(),
                        request_event_id,
                    );
                }
            }
            Mip05GroupMessage::TokenListResponse(response) => {
                let request_event_id = response.request_event_id;
                self.merge_token_list_response(account, &message.mls_group_id, response)
                    .await?;
                self.clear_pending_token_response(
                    &account.pubkey,
                    &message.mls_group_id,
                    &request_event_id,
                );
            }
            Mip05GroupMessage::TokenRemoval(_) => {
                let leaf_index = sender_leaf_index.ok_or_else(|| {
                    WhitenoiseError::InvalidEvent(
                        "MIP-05 token removal missing sender leaf index".to_string(),
                    )
                })?;

                GroupPushToken::delete(
                    &account.pubkey,
                    &message.mls_group_id,
                    leaf_index,
                    &self.database,
                )
                .await?;
            }
        }

        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn has_pending_token_response(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        request_event_id: &EventId,
    ) -> bool {
        self.pending_push_token_responses.contains_key(&(
            *account_pubkey,
            group_id.clone(),
            *request_event_id,
        ))
    }

    pub(crate) fn clear_pending_token_response(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        request_event_id: &EventId,
    ) -> bool {
        self.pending_push_token_responses
            .remove(&(*account_pubkey, group_id.clone(), *request_event_id))
            .is_some()
    }

    pub(crate) async fn dispatch_pending_token_response(
        &self,
        account: &Account,
        group_id: &GroupId,
        request_event_id: EventId,
    ) -> Result<bool> {
        if !self.clear_pending_token_response(&account.pubkey, group_id, &request_event_id) {
            return Ok(false);
        }

        self.respond_to_token_request(account, group_id, request_event_id)
            .await?;
        Ok(true)
    }

    fn schedule_pending_token_response(
        &self,
        account: Account,
        group_id: GroupId,
        request_event_id: EventId,
    ) {
        let key = (account.pubkey, group_id.clone(), request_event_id);
        self.pending_push_token_responses.insert(key, ());

        let context = PendingTokenResponseContext {
            config: self.config.clone(),
            database: Arc::clone(&self.database),
            pending_push_token_responses: Arc::clone(&self.pending_push_token_responses),
            relay_control: Arc::clone(&self.relay_control),
        };
        let delay_ms = ::rand::rng().random_range(1_000..=3_000);

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;

            if let Err(error) = context
                .dispatch_pending_token_response(&account, &group_id, request_event_id)
                .await
            {
                tracing::warn!(
                    target: "whitenoise::push_notifications",
                    account = %account.pubkey.to_hex(),
                    group_id = %hex::encode(group_id.as_slice()),
                    request_event_id = %request_event_id.to_hex(),
                    error = %error,
                    "Failed to send delayed MIP-05 token-list response"
                );
            }
        });
    }

    /// Returns `true` when push-gossip is allowed for the given group:
    /// the MLS group must be active, the account must have an accepted
    /// `AccountGroup` row, and the row must not be marked as removed.
    async fn is_push_gossip_eligible(
        &self,
        account: &Account,
        group_id: &GroupId,
        group_state: GroupState,
    ) -> bool {
        if group_state != GroupState::Active {
            return false;
        }
        match AccountGroup::get(self, &account.pubkey, group_id).await {
            Ok(Some(ag)) => ag.is_accepted() && !ag.is_removed(),
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(
                    target: "whitenoise::push_notifications",
                    account = %account.pubkey.to_hex(),
                    group = %hex::encode(group_id.as_slice()),
                    error = %error,
                    "Failed to check push gossip eligibility; treating as ineligible"
                );
                false
            }
        }
    }

    #[perf_instrument("push_notifications")]
    async fn share_push_token_to_joined_groups(
        &self,
        account: &Account,
        token_tag: &TokenTag,
    ) -> Result<()> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        let groups = mdk.get_groups()?;
        let mut publish_failures = Vec::new();

        for group in groups {
            if !self
                .is_push_gossip_eligible(account, &group.mls_group_id, group.state)
                .await
            {
                continue;
            }

            let rumor = build_token_request_rumor(
                account.pubkey,
                nostr_sdk::Timestamp::now(),
                vec![token_tag.clone()],
            )?;
            if let Err(error) = self
                .publish_push_group_message(account, &group.mls_group_id, rumor)
                .await
            {
                publish_failures.push(format!(
                    "{}: {error}",
                    hex::encode(group.mls_group_id.as_slice())
                ));
                continue;
            }

            if let Err(error) = self
                .sync_local_group_push_token_cache(account, &group.mls_group_id, Some(token_tag))
                .await
            {
                publish_failures.push(format!(
                    "{}: {error}",
                    hex::encode(group.mls_group_id.as_slice())
                ));
            }
        }

        if publish_failures.is_empty() {
            Ok(())
        } else {
            Err(WhitenoiseError::Configuration(format!(
                "failed to share push token to one or more groups: {}",
                publish_failures.join(", ")
            )))
        }
    }

    #[perf_instrument("push_notifications")]
    pub(crate) async fn share_local_push_token_to_joined_groups(
        &self,
        account: &Account,
    ) -> Result<()> {
        if !AccountSettings::notifications_enabled_for_pubkey(&account.pubkey, &self.database)
            .await?
        {
            return Ok(());
        }

        let Some(token_tag) = self.local_push_token_tag(account).await? else {
            return Ok(());
        };

        self.share_push_token_to_joined_groups(account, &token_tag)
            .await
    }

    #[perf_instrument("push_notifications")]
    pub(crate) async fn reconcile_group_push_tokens_for_active_leaves(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<()> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        let active_leaf_map = mdk.group_leaf_map(group_id)?;
        let cached_tokens =
            GroupPushToken::find_by_account_and_group(&account.pubkey, group_id, &self.database)
                .await?;

        for token in cached_tokens {
            match active_leaf_map.get(&token.leaf_index) {
                Some(active_member_pubkey) if active_member_pubkey == &token.member_pubkey => {
                    continue;
                }
                Some(_) => {
                    GroupPushToken::delete(
                        &account.pubkey,
                        group_id,
                        token.leaf_index,
                        &self.database,
                    )
                    .await?;
                }
                None => {
                    let member_still_active = active_leaf_map
                        .values()
                        .any(|member_pubkey| member_pubkey == &token.member_pubkey);

                    if member_still_active {
                        GroupPushToken::delete(
                            &account.pubkey,
                            group_id,
                            token.leaf_index,
                            &self.database,
                        )
                        .await?;
                    } else {
                        GroupPushToken::delete_by_member_pubkey(
                            &account.pubkey,
                            group_id,
                            &token.member_pubkey,
                            &self.database,
                        )
                        .await?;
                    }
                }
            }
        }

        Ok(())
    }

    #[perf_instrument("push_notifications")]
    async fn share_push_token_to_group(
        &self,
        account: &Account,
        group_id: &GroupId,
        token_tag: &TokenTag,
    ) -> Result<()> {
        let rumor = build_token_request_rumor(
            account.pubkey,
            nostr_sdk::Timestamp::now(),
            vec![token_tag.clone()],
        )?;
        self.publish_push_group_message(account, group_id, rumor)
            .await?;
        self.sync_local_group_push_token_cache(account, group_id, Some(token_tag))
            .await
    }

    #[perf_instrument("push_notifications")]
    pub(crate) async fn share_local_push_token_to_group(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<()> {
        // Only share to groups the user has explicitly accepted.
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        let group_state = mdk
            .get_group(group_id)?
            .map(|g| g.state)
            .unwrap_or(GroupState::Inactive);
        if !self
            .is_push_gossip_eligible(account, group_id, group_state)
            .await
        {
            return Ok(());
        }

        if !AccountSettings::notifications_enabled_for_pubkey(&account.pubkey, &self.database)
            .await?
        {
            return Ok(());
        }

        let Some(token_tag) = self.local_push_token_tag(account).await? else {
            return Ok(());
        };

        self.share_push_token_to_group(account, group_id, &token_tag)
            .await
    }

    #[perf_instrument("push_notifications")]
    pub(crate) async fn remove_local_push_token_from_joined_groups(
        &self,
        account: &Account,
    ) -> Result<()> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        let groups = mdk.get_groups()?;
        let mut publish_failures = Vec::new();

        for group in groups {
            if group.state != GroupState::Active {
                continue;
            }

            let rumor = build_token_removal_rumor(account.pubkey, nostr_sdk::Timestamp::now());
            if let Err(error) = self
                .publish_push_group_message(account, &group.mls_group_id, rumor)
                .await
            {
                publish_failures.push(format!(
                    "{}: {error}",
                    hex::encode(group.mls_group_id.as_slice())
                ));
            }

            if let Err(error) = self
                .sync_local_group_push_token_cache(account, &group.mls_group_id, None)
                .await
            {
                publish_failures.push(format!(
                    "{}: {error}",
                    hex::encode(group.mls_group_id.as_slice())
                ));
            }
        }

        if publish_failures.is_empty() {
            Ok(())
        } else {
            Err(WhitenoiseError::Configuration(format!(
                "failed to remove push token from one or more groups: {}",
                publish_failures.join(", ")
            )))
        }
    }

    /// Removes the local push token from a single group and clears
    /// the sender's own cached leaf token for that group.
    #[perf_instrument("push_notifications")]
    pub(crate) async fn remove_local_push_token_from_group(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<()> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        let group_state = mdk
            .get_group(group_id)?
            .map(|g| g.state)
            .unwrap_or(GroupState::Inactive);

        if group_state != GroupState::Active {
            // Group is missing or inactive — clear any stale cache rows directly
            // since own_leaf_index() is unavailable without an active MLS group.
            GroupPushToken::delete_by_member_pubkey(
                &account.pubkey,
                group_id,
                &account.pubkey,
                &self.database,
            )
            .await?;
            return Ok(());
        }

        let rumor = build_token_removal_rumor(account.pubkey, nostr_sdk::Timestamp::now());
        if let Err(error) = self
            .publish_push_group_message(account, group_id, rumor)
            .await
        {
            tracing::warn!(
                target: "whitenoise::push_notifications",
                account = %account.pubkey.to_hex(),
                group = %hex::encode(group_id.as_slice()),
                error = %error,
                "Failed to publish token removal to group"
            );
        }

        self.sync_local_group_push_token_cache(account, group_id, None)
            .await
    }

    #[perf_instrument("push_notifications")]
    async fn merge_token_request(
        &self,
        account: &Account,
        mls_group_id: &GroupId,
        member_pubkey: PublicKey,
        leaf_index: u32,
        request: mdk_core::mip05::TokenRequest,
    ) -> Result<()> {
        let Some(token) = request.tokens.into_iter().next() else {
            return Err(WhitenoiseError::InvalidEvent(
                "MIP-05 token request must include at least one token".to_string(),
            ));
        };

        GroupPushToken::upsert(
            &account.pubkey,
            mls_group_id,
            &member_pubkey,
            leaf_index,
            &token.server_pubkey,
            Some(&token.relay_hint),
            &token.encrypted_token.to_base64(),
            &self.database,
        )
        .await?;

        Ok(())
    }

    #[perf_instrument("push_notifications")]
    async fn merge_token_list_response(
        &self,
        account: &Account,
        mls_group_id: &GroupId,
        response: mdk_core::mip05::TokenListResponse,
    ) -> Result<()> {
        let active_leaf_map = self
            .create_mdk_for_account(account.pubkey)?
            .group_leaf_map(mls_group_id)?;

        GroupPushToken::upsert_active_token_list_response(
            &account.pubkey,
            mls_group_id,
            &active_leaf_map,
            response.tokens,
            &self.database,
        )
        .await?;

        Ok(())
    }

    #[perf_instrument("push_notifications")]
    async fn respond_to_token_request(
        &self,
        account: &Account,
        group_id: &GroupId,
        request_event_id: EventId,
    ) -> Result<()> {
        let token_tags = self
            .group_push_token_tags_for_response(account, group_id)
            .await?;

        if token_tags.is_empty() {
            return Ok(());
        }

        let rumor = build_token_list_response_rumor(
            account.pubkey,
            nostr_sdk::Timestamp::now(),
            request_event_id,
            token_tags,
        )?;
        self.publish_push_group_message(account, group_id, rumor)
            .await
    }

    #[perf_instrument("push_notifications")]
    async fn group_push_token_tags_for_response(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<Vec<LeafTokenTag>> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        group_push_token_tags_for_response_with(&account.pubkey, group_id, &self.database, &mdk)
            .await
    }

    #[perf_instrument("push_notifications")]
    async fn sync_local_group_push_token_cache(
        &self,
        account: &Account,
        group_id: &GroupId,
        token_tag: Option<&TokenTag>,
    ) -> Result<()> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        let leaf_index = mdk.own_leaf_index(group_id)?;

        match token_tag {
            Some(token_tag) => {
                GroupPushToken::upsert(
                    &account.pubkey,
                    group_id,
                    &account.pubkey,
                    leaf_index,
                    &token_tag.server_pubkey,
                    Some(&token_tag.relay_hint),
                    &token_tag.encrypted_token.to_base64(),
                    &self.database,
                )
                .await?;
            }
            None => {
                GroupPushToken::delete(&account.pubkey, group_id, leaf_index, &self.database)
                    .await?;
            }
        }

        Ok(())
    }

    #[perf_instrument("push_notifications")]
    async fn local_push_token_tag(&self, account: &Account) -> Result<Option<TokenTag>> {
        let Some(registration) = self.push_registration(account).await? else {
            return Ok(None);
        };

        registration.token_tag()
    }

    #[perf_instrument("push_notifications")]
    async fn publish_push_group_message(
        &self,
        account: &Account,
        group_id: &GroupId,
        rumor: nostr_sdk::UnsignedEvent,
    ) -> Result<()> {
        publish_push_group_message_with(&self.config, &self.relay_control, account, group_id, rumor)
            .await
    }
}

fn validate_raw_token(raw_token: &str) -> Result<()> {
    if raw_token.trim().is_empty() {
        return Err(WhitenoiseError::InvalidInput(
            "push registration token must not be empty".to_string(),
        ));
    }

    Ok(())
}

impl PushRegistration {
    fn token_tag(&self) -> Result<Option<TokenTag>> {
        let plaintext = self.push_token_plaintext()?;
        let Some(relay_hint) = self.relay_hint.clone() else {
            return Ok(None);
        };

        let encrypted_token = encrypt_push_token(&self.server_pubkey, &plaintext)?;

        Ok(Some(TokenTag {
            encrypted_token,
            server_pubkey: self.server_pubkey,
            relay_hint,
        }))
    }

    fn push_token_plaintext(&self) -> Result<PushTokenPlaintext> {
        match self.platform {
            PushPlatform::Apns => {
                // iOS tokens are 32 raw bytes, but some app layers surface them as
                // 64-character hex strings, so accept either representation.
                let token_bytes = if self.raw_token.len() == 64 {
                    hex::decode(&self.raw_token).map_err(|error| {
                        WhitenoiseError::InvalidInput(format!(
                            "invalid APNs token hex encoding: {error}"
                        ))
                    })?
                } else if self.raw_token.len() == 32 {
                    self.raw_token.as_bytes().to_vec()
                } else {
                    return Err(WhitenoiseError::InvalidInput(
                        "APNs token must be 32 raw bytes or 64 hex characters".to_string(),
                    ));
                };

                PushTokenPlaintext::new(NotificationPlatform::Apns, token_bytes)
                    .map_err(WhitenoiseError::from)
            }
            PushPlatform::Fcm => PushTokenPlaintext::new(
                NotificationPlatform::Fcm,
                self.raw_token.as_bytes().to_vec(),
            )
            .map_err(WhitenoiseError::from),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, time::Duration};

    use mdk_core::mip05::parse_notification_request_rumor;
    use mdk_core::prelude::NostrGroupDataUpdate;
    use nostr_sdk::{Filter, Keys, Kind, RelayUrl, Tag, nips::nip59};

    use super::*;
    use crate::relay_control::{
        ephemeral::{EphemeralPlane, EphemeralPlaneConfig},
        sessions::{RelaySessionAuthPolicy, RelaySessionReconnectPolicy},
    };
    use crate::whitenoise::group_information::GroupType;
    use crate::whitenoise::test_utils::{
        count_published_events_for_account, create_mock_whitenoise, create_nostr_group_config_data,
        setup_multiple_test_accounts, setup_two_member_group_with_accepted_account_groups,
        setup_two_member_group_with_welcome_finalization, wait_for_exact_published_event_count,
        wait_for_key_package_publication, wait_for_published_event_count,
    };
    use crate::whitenoise::{
        aggregated_message::AggregatedMessage, message_aggregator::DeliveryStatus, relays::Relay,
    };

    fn encrypted_fcm_token_base64(server_pubkey: &PublicKey, raw_token: &str) -> String {
        let plaintext =
            PushTokenPlaintext::new(NotificationPlatform::Fcm, raw_token.as_bytes().to_vec())
                .unwrap();

        encrypt_push_token(server_pubkey, &plaintext)
            .unwrap()
            .to_base64()
    }

    fn test_group_push_token(
        account_pubkey: PublicKey,
        group_id: GroupId,
        member_pubkey: PublicKey,
        leaf_index: u32,
        server_pubkey: PublicKey,
        relay_hint: Option<RelayUrl>,
        encrypted_token: String,
    ) -> GroupPushToken {
        GroupPushToken {
            account_pubkey,
            mls_group_id: group_id,
            member_pubkey,
            leaf_index,
            server_pubkey,
            relay_hint,
            encrypted_token,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    async fn publish_notification_requests_after_delivery_for_test(
        whitenoise: &Whitenoise,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<()> {
        let user_relay_sync = whitenoise.user_relay_sync_context();
        let ephemeral = whitenoise.relay_control.ephemeral();

        publish_notification_requests_after_delivery_with(
            &whitenoise.config,
            &whitenoise.database,
            &user_relay_sync,
            &ephemeral,
            account.pubkey,
            group_id,
        )
        .await
    }

    async fn wait_for_user_inbox_relays_on_network(
        whitenoise: &Whitenoise,
        user_pubkey: PublicKey,
        query_relays: &[RelayUrl],
    ) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if whitenoise
                    .relay_control
                    .fetch_user_relays(user_pubkey, RelayType::Inbox, query_relays)
                    .await
                    .unwrap()
                    .is_some()
                {
                    break;
                }

                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timed out waiting for inbox relay list publication");
    }

    async fn wait_for_notification_giftwraps(
        whitenoise: &Whitenoise,
        relay_urls: &[RelayUrl],
        receiver_pubkey: PublicKey,
        expected_count: usize,
    ) -> Vec<nostr_sdk::Event> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let events = whitenoise
                    .relay_control
                    .ephemeral()
                    .fetch_events_from(
                        relay_urls,
                        Filter::new().kind(Kind::GiftWrap).pubkey(receiver_pubkey),
                    )
                    .await
                    .unwrap();
                let mut unique_events = Vec::new();
                let mut seen_ids = std::collections::HashSet::new();

                for event in events {
                    if seen_ids.insert(event.id) {
                        unique_events.push(event);
                    }
                }

                if unique_events.len() >= expected_count {
                    return unique_events;
                }

                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("timed out waiting for notification gift wraps")
    }

    async fn wait_for_message_sent_status(
        whitenoise: &Whitenoise,
        group_id: &GroupId,
        message_id: &EventId,
    ) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let message = AggregatedMessage::find_by_id(
                    &message_id.to_string(),
                    group_id,
                    &whitenoise.database,
                )
                .await
                .unwrap()
                .expect("message should stay cached");

                if matches!(message.delivery_status, Some(DeliveryStatus::Sent(_))) {
                    break;
                }

                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timed out waiting for Sent delivery status");
    }

    fn test_ephemeral_plane(whitenoise: &Whitenoise) -> EphemeralPlane {
        EphemeralPlane::new(
            EphemeralPlaneConfig {
                timeout: Duration::from_millis(200),
                reconnect_policy: RelaySessionReconnectPolicy::Disabled,
                auth_policy: RelaySessionAuthPolicy::Disabled,
                max_publish_attempts: 1,
                ad_hoc_relay_ttl: Duration::from_secs(30),
            },
            whitenoise.database.clone(),
            whitenoise.event_sender.clone(),
            whitenoise.relay_control.observability().clone(),
        )
    }

    #[test]
    fn test_dedupe_relay_urls_preserves_first_seen_order() {
        let relay_a = RelayUrl::parse("wss://relay-a.example.com").unwrap();
        let relay_b = RelayUrl::parse("wss://relay-b.example.com").unwrap();
        let relay_c = RelayUrl::parse("wss://relay-c.example.com").unwrap();
        let deduped = dedupe_relay_urls(&[
            relay_a.clone(),
            relay_b.clone(),
            relay_a.clone(),
            relay_c.clone(),
            relay_b.clone(),
        ]);

        assert_eq!(deduped, vec![relay_a, relay_b, relay_c]);
    }

    #[test]
    fn test_prepare_notification_recipient_tokens_selects_valid_non_sender_tokens() {
        let account_pubkey = Keys::generate().public_key();
        let sender_other_device_pubkey = account_pubkey;
        let recipient_pubkey = Keys::generate().public_key();
        let mismatched_pubkey = Keys::generate().public_key();
        let inactive_pubkey = Keys::generate().public_key();
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let group_id = GroupId::from_slice(&[42; 32]);
        let active_leaf_map = BTreeMap::from([
            (1u32, account_pubkey),
            (2u32, sender_other_device_pubkey),
            (3u32, recipient_pubkey),
            (4u32, mismatched_pubkey),
        ]);
        let cached_tokens = vec![
            test_group_push_token(
                account_pubkey,
                group_id.clone(),
                account_pubkey,
                1,
                server_pubkey,
                Some(relay_hint.clone()),
                encrypted_fcm_token_base64(&server_pubkey, "sender-primary"),
            ),
            test_group_push_token(
                account_pubkey,
                group_id.clone(),
                sender_other_device_pubkey,
                2,
                server_pubkey,
                Some(relay_hint.clone()),
                encrypted_fcm_token_base64(&server_pubkey, "sender-secondary"),
            ),
            test_group_push_token(
                account_pubkey,
                group_id.clone(),
                recipient_pubkey,
                3,
                server_pubkey,
                Some(relay_hint.clone()),
                encrypted_fcm_token_base64(&server_pubkey, "recipient-device"),
            ),
            test_group_push_token(
                account_pubkey,
                group_id.clone(),
                recipient_pubkey,
                3,
                server_pubkey,
                None,
                encrypted_fcm_token_base64(&server_pubkey, "missing-relay-hint"),
            ),
            test_group_push_token(
                account_pubkey,
                group_id.clone(),
                recipient_pubkey,
                3,
                server_pubkey,
                Some(relay_hint.clone()),
                "not-base64".to_string(),
            ),
            test_group_push_token(
                account_pubkey,
                group_id.clone(),
                recipient_pubkey,
                4,
                server_pubkey,
                Some(relay_hint.clone()),
                encrypted_fcm_token_base64(&server_pubkey, "mismatched-member"),
            ),
            test_group_push_token(
                account_pubkey,
                group_id,
                inactive_pubkey,
                9,
                server_pubkey,
                Some(relay_hint.clone()),
                encrypted_fcm_token_base64(&server_pubkey, "inactive-member"),
            ),
        ];

        let recipient_tokens = prepare_notification_recipient_tokens(
            &account_pubkey,
            &cached_tokens,
            &active_leaf_map,
            &account_pubkey,
        );

        assert_eq!(recipient_tokens.len(), 1);
        assert_eq!(recipient_tokens[0].server_pubkey, server_pubkey);
        assert_eq!(recipient_tokens[0].relay_hint, relay_hint);
    }

    #[test]
    fn test_prepare_notification_recipient_tokens_dedupes_duplicate_tokens_per_server() {
        let account_pubkey = Keys::generate().public_key();
        let recipient_pubkey = Keys::generate().public_key();
        let second_recipient_pubkey = Keys::generate().public_key();
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let group_id = GroupId::from_slice(&[7; 32]);
        let duplicated_token = encrypted_fcm_token_base64(&server_pubkey, "same-device");
        let active_leaf_map = BTreeMap::from([
            (1u32, account_pubkey),
            (2u32, recipient_pubkey),
            (3u32, second_recipient_pubkey),
        ]);
        let cached_tokens = vec![
            test_group_push_token(
                account_pubkey,
                group_id.clone(),
                recipient_pubkey,
                2,
                server_pubkey,
                Some(relay_hint.clone()),
                duplicated_token.clone(),
            ),
            test_group_push_token(
                account_pubkey,
                group_id,
                second_recipient_pubkey,
                3,
                server_pubkey,
                Some(relay_hint),
                duplicated_token,
            ),
        ];

        let recipient_tokens = prepare_notification_recipient_tokens(
            &account_pubkey,
            &cached_tokens,
            &active_leaf_map,
            &account_pubkey,
        );

        assert_eq!(recipient_tokens.len(), 1);
    }

    #[test]
    fn test_prepare_notification_recipient_tokens_empty_cache_is_noop() {
        let account_pubkey = Keys::generate().public_key();
        let active_leaf_map = BTreeMap::from([(1u32, account_pubkey)]);

        let recipient_tokens = prepare_notification_recipient_tokens(
            &account_pubkey,
            &[],
            &active_leaf_map,
            &account_pubkey,
        );

        assert!(recipient_tokens.is_empty());
    }

    #[tokio::test]
    async fn test_resolve_notification_server_relays_prefers_cached_inbox_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let server_pubkey = Keys::generate().public_key();
        let relay_url = RelayUrl::parse("wss://cached-push.example.com").unwrap();
        let (server_user, _created) = crate::whitenoise::users::User::find_or_create_by_pubkey(
            &server_pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap();
        let relay = Relay::find_or_create_by_url(&relay_url, &whitenoise.database)
            .await
            .unwrap();

        server_user
            .add_relay(&relay, RelayType::Inbox, &whitenoise.database)
            .await
            .unwrap();

        let (resolved_relays, relay_source) = resolve_notification_server_relays(
            &whitenoise.database,
            &whitenoise.user_relay_sync_context(),
            server_pubkey,
            &[],
        )
        .await
        .unwrap();

        assert_eq!(resolved_relays, vec![relay_url]);
        assert_eq!(relay_source, NotificationRelaySource::Cached);
    }

    #[tokio::test]
    async fn test_resolve_notification_server_relays_ignores_hints_when_cached_relays_exist() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let server_pubkey = Keys::generate().public_key();
        let cached_relay = RelayUrl::parse("wss://cached-push.example.com").unwrap();
        let fresh_hint = RelayUrl::parse("wss://fresh-hint.example.com").unwrap();
        let duplicate_cached_hint = RelayUrl::parse("wss://cached-push.example.com").unwrap();
        let (server_user, _created) = crate::whitenoise::users::User::find_or_create_by_pubkey(
            &server_pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap();
        let relay = Relay::find_or_create_by_url(&cached_relay, &whitenoise.database)
            .await
            .unwrap();

        server_user
            .add_relay(&relay, RelayType::Inbox, &whitenoise.database)
            .await
            .unwrap();

        let (resolved_relays, relay_source) = resolve_notification_server_relays(
            &whitenoise.database,
            &whitenoise.user_relay_sync_context(),
            server_pubkey,
            &[duplicate_cached_hint, fresh_hint.clone()],
        )
        .await
        .unwrap();

        assert_eq!(resolved_relays, vec![cached_relay.clone()]);
        assert_eq!(relay_source, NotificationRelaySource::Cached);
        assert_eq!(
            Relay::urls(
                &server_user
                    .relays(RelayType::Inbox, &whitenoise.database)
                    .await
                    .unwrap()
            ),
            vec![cached_relay]
        );
    }

    #[tokio::test]
    async fn test_resolve_notification_server_relays_without_hints_returns_empty_fallback() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let server_pubkey = Keys::generate().public_key();

        let (resolved_relays, relay_source) = resolve_notification_server_relays(
            &whitenoise.database,
            &whitenoise.user_relay_sync_context(),
            server_pubkey,
            &[],
        )
        .await
        .unwrap();

        assert!(resolved_relays.is_empty());
        assert_eq!(relay_source, NotificationRelaySource::HintFallback);

        let server_user =
            crate::whitenoise::users::User::find_by_pubkey(&server_pubkey, &whitenoise.database)
                .await
                .unwrap();
        assert!(
            server_user
                .relays(RelayType::Inbox, &whitenoise.database)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_publish_notification_batches_best_effort_skips_when_no_server_relays_exist() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let sender_pubkey = Keys::generate().public_key();
        let server_pubkey = Keys::generate().public_key();
        let token_tags = vec![TokenTag {
            encrypted_token: encrypt_push_token(
                &server_pubkey,
                &PushTokenPlaintext::new(NotificationPlatform::Fcm, b"candidate-token".to_vec())
                    .unwrap(),
            )
            .unwrap(),
            server_pubkey,
            relay_hint: RelayUrl::parse("wss://ignored-hint.example.com").unwrap(),
        }];
        let mut batches = build_notification_batches(token_tags.clone()).unwrap();

        batches[0].relay_hints.clear();

        publish_notification_batches_best_effort(
            &whitenoise.database,
            &whitenoise.user_relay_sync_context(),
            &test_ephemeral_plane(&whitenoise),
            sender_pubkey,
            &notification_token_counts_by_server(&token_tags),
            batches,
        )
        .await;

        let server_user =
            crate::whitenoise::users::User::find_by_pubkey(&server_pubkey, &whitenoise.database)
                .await
                .unwrap();
        assert!(
            server_user
                .relays(RelayType::Inbox, &whitenoise.database)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_publish_notification_requests_after_delivery_wrapper_is_noop_without_cache() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let mut members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members.remove(0).0;

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id = setup_two_member_group_with_welcome_finalization(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;
        publish_notification_requests_after_delivery_for_test(
            &whitenoise,
            &admin_account,
            &group_id,
        )
        .await
        .unwrap();

        let cached_tokens = GroupPushToken::find_by_account_and_group(
            &admin_account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(cached_tokens.is_empty());
    }

    #[tokio::test]
    async fn test_publish_notification_requests_after_delivery_publishes_expected_446_tokens() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let mut accounts = setup_multiple_test_accounts(&whitenoise, 2).await;
        let (member_account, _member_keys) = accounts.remove(0);
        let (server_account, server_keys) = accounts.remove(0);

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id = setup_two_member_group_with_welcome_finalization(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;
        let server_inbox_relays = server_account.inbox_relays(&whitenoise).await.unwrap();
        let server_relay_urls = Relay::urls(&server_inbox_relays);

        wait_for_user_inbox_relays_on_network(
            &whitenoise,
            server_account.pubkey,
            &server_relay_urls,
        )
        .await;

        let server_user = whitenoise
            .find_user_by_pubkey(&server_account.pubkey)
            .await
            .unwrap();
        server_user
            .remove_all_relays(&whitenoise.database)
            .await
            .unwrap();

        let admin_mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();
        let active_leaf_map = admin_mdk.group_leaf_map(&group_id).unwrap();
        let admin_leaf_index = admin_mdk.own_leaf_index(&group_id).unwrap();
        let member_leaf_index = active_leaf_map
            .iter()
            .find_map(|(leaf_index, pubkey)| {
                (*pubkey == member_account.pubkey).then_some(*leaf_index)
            })
            .unwrap();
        let sender_token = encrypted_fcm_token_base64(&server_account.pubkey, "sender-own-device");
        let recipient_token = encrypt_push_token(
            &server_account.pubkey,
            &PushTokenPlaintext::new(NotificationPlatform::Fcm, b"recipient-device".to_vec())
                .unwrap(),
        )
        .unwrap();

        GroupPushToken::upsert(
            &admin_account.pubkey,
            &group_id,
            &admin_account.pubkey,
            admin_leaf_index,
            &server_account.pubkey,
            Some(&server_relay_urls[0]),
            &sender_token,
            &whitenoise.database,
        )
        .await
        .unwrap();
        GroupPushToken::upsert(
            &admin_account.pubkey,
            &group_id,
            &member_account.pubkey,
            member_leaf_index,
            &server_account.pubkey,
            Some(&server_relay_urls[0]),
            &recipient_token.to_base64(),
            &whitenoise.database,
        )
        .await
        .unwrap();

        whitenoise
            .send_message_to_group(
                &admin_account,
                &group_id,
                "hello relay-backed notifications".to_string(),
                9,
                None,
            )
            .await
            .unwrap();

        let giftwraps = wait_for_notification_giftwraps(
            &whitenoise,
            &server_relay_urls,
            server_account.pubkey,
            1,
        )
        .await;
        let unwrapped = nip59::extract_rumor(&server_keys, &giftwraps[0])
            .await
            .unwrap();
        let parsed = parse_notification_request_rumor(&unwrapped.rumor).unwrap();

        assert_eq!(parsed.tokens, vec![recipient_token]);
    }

    #[tokio::test]
    async fn test_reaction_delivery_publishes_expected_446_tokens() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let mut accounts = setup_multiple_test_accounts(&whitenoise, 2).await;
        let (member_account, _member_keys) = accounts.remove(0);
        let (server_account, server_keys) = accounts.remove(0);

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id = setup_two_member_group_with_welcome_finalization(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;
        let server_inbox_relays = server_account.inbox_relays(&whitenoise).await.unwrap();
        let server_relay_urls = Relay::urls(&server_inbox_relays);

        wait_for_user_inbox_relays_on_network(
            &whitenoise,
            server_account.pubkey,
            &server_relay_urls,
        )
        .await;

        let server_user = whitenoise
            .find_user_by_pubkey(&server_account.pubkey)
            .await
            .unwrap();
        server_user
            .remove_all_relays(&whitenoise.database)
            .await
            .unwrap();

        let chat_result = whitenoise
            .send_message_to_group(
                &admin_account,
                &group_id,
                "reaction target".to_string(),
                9,
                None,
            )
            .await
            .unwrap();
        wait_for_message_sent_status(&whitenoise, &group_id, &chat_result.message.id).await;

        let admin_mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();
        let active_leaf_map = admin_mdk.group_leaf_map(&group_id).unwrap();
        let admin_leaf_index = admin_mdk.own_leaf_index(&group_id).unwrap();
        let member_leaf_index = active_leaf_map
            .iter()
            .find_map(|(leaf_index, pubkey)| {
                (*pubkey == member_account.pubkey).then_some(*leaf_index)
            })
            .unwrap();
        let sender_token = encrypted_fcm_token_base64(&server_account.pubkey, "sender-own-device");
        let recipient_token = encrypt_push_token(
            &server_account.pubkey,
            &PushTokenPlaintext::new(NotificationPlatform::Fcm, b"recipient-device".to_vec())
                .unwrap(),
        )
        .unwrap();

        GroupPushToken::upsert(
            &admin_account.pubkey,
            &group_id,
            &admin_account.pubkey,
            admin_leaf_index,
            &server_account.pubkey,
            Some(&server_relay_urls[0]),
            &sender_token,
            &whitenoise.database,
        )
        .await
        .unwrap();
        GroupPushToken::upsert(
            &admin_account.pubkey,
            &group_id,
            &member_account.pubkey,
            member_leaf_index,
            &server_account.pubkey,
            Some(&server_relay_urls[0]),
            &recipient_token.to_base64(),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let reaction_tags = Some(vec![
            Tag::parse(vec!["e", &chat_result.message.id.to_hex()]).unwrap(),
        ]);
        let reaction_result = whitenoise
            .send_message_to_group(&admin_account, &group_id, "+".to_string(), 7, reaction_tags)
            .await
            .unwrap();
        assert_eq!(reaction_result.message.kind.as_u16(), 7);

        let giftwraps = wait_for_notification_giftwraps(
            &whitenoise,
            &server_relay_urls,
            server_account.pubkey,
            1,
        )
        .await;
        let unwrapped = nip59::extract_rumor(&server_keys, &giftwraps[0])
            .await
            .unwrap();
        let parsed = parse_notification_request_rumor(&unwrapped.rumor).unwrap();

        assert_eq!(parsed.tokens, vec![recipient_token]);
    }

    #[tokio::test]
    async fn test_publish_notification_batches_best_effort_publishes_multiple_1059_events_for_large_batch()
     {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let sender_account = whitenoise.create_identity().await.unwrap();
        let mut server_accounts = setup_multiple_test_accounts(&whitenoise, 1).await;
        let (server_account, server_keys) = server_accounts.remove(0);
        let server_inbox_relays = server_account.inbox_relays(&whitenoise).await.unwrap();
        let server_relay_urls = Relay::urls(&server_inbox_relays);

        wait_for_user_inbox_relays_on_network(
            &whitenoise,
            server_account.pubkey,
            &server_relay_urls,
        )
        .await;

        let server_user = whitenoise
            .find_user_by_pubkey(&server_account.pubkey)
            .await
            .unwrap();
        server_user
            .remove_all_relays(&whitenoise.database)
            .await
            .unwrap();

        let relay_hint = server_relay_urls[0].clone();
        let token_tags: Vec<TokenTag> = (0..(mdk_core::mip05::MAX_NOTIFICATION_REQUEST_TOKENS + 1))
            .map(|index| TokenTag {
                encrypted_token: encrypt_push_token(
                    &server_account.pubkey,
                    &PushTokenPlaintext::new(
                        NotificationPlatform::Fcm,
                        format!("candidate-token-{index}").into_bytes(),
                    )
                    .unwrap(),
                )
                .unwrap(),
                server_pubkey: server_account.pubkey,
                relay_hint: relay_hint.clone(),
            })
            .collect();
        let batches = build_notification_batches(token_tags.clone()).unwrap();

        assert_eq!(batches.len(), 1);
        assert!(batches[0].events.len() > 1);

        publish_notification_batches_best_effort(
            &whitenoise.database,
            &whitenoise.user_relay_sync_context(),
            &whitenoise.relay_control.ephemeral(),
            sender_account.pubkey,
            &notification_token_counts_by_server(&token_tags),
            batches,
        )
        .await;

        let giftwraps = wait_for_notification_giftwraps(
            &whitenoise,
            &server_relay_urls,
            server_account.pubkey,
            2,
        )
        .await;
        let mut received_tokens = Vec::new();

        for giftwrap in giftwraps {
            let unwrapped = nip59::extract_rumor(&server_keys, &giftwrap).await.unwrap();
            let parsed = parse_notification_request_rumor(&unwrapped.rumor).unwrap();
            received_tokens.extend(parsed.tokens);
        }

        assert_eq!(received_tokens.len(), token_tags.len());
    }

    #[tokio::test]
    async fn test_notification_publish_failure_does_not_change_chat_delivery_status() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let mut member_accounts = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member = member_accounts.remove(0).0;

        wait_for_key_package_publication(&whitenoise, &[&member]).await;

        let group_id =
            setup_two_member_group_with_welcome_finalization(&whitenoise, &creator, &member).await;
        let send_result = whitenoise
            .send_message_to_group(&creator, &group_id, "status isolation".to_string(), 9, None)
            .await
            .unwrap();

        wait_for_message_sent_status(&whitenoise, &group_id, &send_result.message.id).await;

        let creator_mdk = whitenoise.create_mdk_for_account(creator.pubkey).unwrap();
        let active_leaf_map = creator_mdk.group_leaf_map(&group_id).unwrap();
        let member_leaf_index = active_leaf_map
            .iter()
            .find_map(|(leaf_index, pubkey)| (*pubkey == member.pubkey).then_some(*leaf_index))
            .unwrap();
        let unreachable_server_pubkey = Keys::generate().public_key();
        let unreachable_relay = RelayUrl::parse("ws://127.0.0.1:1").unwrap();

        GroupPushToken::upsert(
            &creator.pubkey,
            &group_id,
            &member.pubkey,
            member_leaf_index,
            &unreachable_server_pubkey,
            Some(&unreachable_relay),
            &encrypted_fcm_token_base64(&unreachable_server_pubkey, "bad-recipient-device"),
            &whitenoise.database,
        )
        .await
        .unwrap();
        let ephemeral = test_ephemeral_plane(&whitenoise);
        let user_relay_sync = whitenoise.user_relay_sync_context();

        publish_notification_requests_after_delivery_with(
            &whitenoise.config,
            &whitenoise.database,
            &user_relay_sync,
            &ephemeral,
            creator.pubkey,
            &group_id,
        )
        .await
        .unwrap();

        let cached_message = AggregatedMessage::find_by_id(
            &send_result.message.id.to_string(),
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();

        assert!(matches!(
            cached_message.delivery_status,
            Some(DeliveryStatus::Sent(_))
        ));
    }

    #[tokio::test]
    async fn test_public_push_registration_lifecycle() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();

        assert!(
            whitenoise
                .push_registration(&account)
                .await
                .unwrap()
                .is_none()
        );

        let created = whitenoise
            .upsert_push_registration(
                &account,
                PushPlatform::Apns,
                &"aa".repeat(32),
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();
        assert_eq!(created.platform, PushPlatform::Apns);
        assert_eq!(created.raw_token, "aa".repeat(32));
        assert_eq!(created.server_pubkey, server_pubkey);
        assert_eq!(created.relay_hint, Some(relay_hint.clone()));

        let replaced = whitenoise
            .upsert_push_registration(
                &account,
                PushPlatform::Fcm,
                "token-two",
                &server_pubkey,
                None,
            )
            .await
            .unwrap();
        assert_eq!(replaced.platform, PushPlatform::Fcm);
        assert_eq!(replaced.raw_token, "token-two");
        assert_eq!(replaced.relay_hint, None);

        let stored = whitenoise
            .push_registration(&account)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored, replaced);

        whitenoise.clear_push_registration(&account).await.unwrap();
        assert!(
            whitenoise
                .push_registration(&account)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_registration_remains_stored_when_notifications_disabled() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let server_pubkey = Keys::generate().public_key();

        whitenoise
            .upsert_push_registration(
                &account,
                PushPlatform::Fcm,
                "device-token",
                &server_pubkey,
                None,
            )
            .await
            .unwrap();

        let settings = whitenoise
            .update_notifications_enabled(&account, false)
            .await
            .unwrap();
        assert!(!settings.notifications_enabled);

        let stored = whitenoise.push_registration(&account).await.unwrap();
        assert!(stored.is_some());
        assert_eq!(stored.unwrap().raw_token, "device-token");
    }

    #[tokio::test]
    async fn test_upsert_push_registration_rejects_blank_token() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let server_pubkey = Keys::generate().public_key();

        let err = whitenoise
            .upsert_push_registration(&account, PushPlatform::Apns, "   ", &server_pubkey, None)
            .await
            .unwrap_err();

        assert!(matches!(err, WhitenoiseError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn test_upsert_push_registration_shares_to_joined_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id = setup_two_member_group_with_accepted_account_groups(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;
        let before_count = count_published_events_for_account(&whitenoise, &admin_account).await;
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let apns_hex_token = "11".repeat(32);

        whitenoise
            .upsert_push_registration(
                &admin_account,
                PushPlatform::Apns,
                &apns_hex_token,
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();

        let after_count =
            wait_for_published_event_count(&whitenoise, &admin_account, before_count).await;
        assert!(after_count > before_count);

        let own_leaf_index = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap()
            .own_leaf_index(&group_id)
            .unwrap();
        let cached_tokens = GroupPushToken::find_by_account_and_group(
            &admin_account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(
            cached_tokens
                .iter()
                .any(|token| token.leaf_index == own_leaf_index),
            "local cache should include the sender's own shared token after publish"
        );
    }

    #[tokio::test]
    async fn test_disabling_notifications_publishes_token_removal() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        setup_two_member_group_with_accepted_account_groups(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let apns_hex_token = "22".repeat(32);

        let before_share_count =
            count_published_events_for_account(&whitenoise, &admin_account).await;

        whitenoise
            .upsert_push_registration(
                &admin_account,
                PushPlatform::Apns,
                &apns_hex_token,
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();

        let after_share_count =
            wait_for_published_event_count(&whitenoise, &admin_account, before_share_count).await;
        assert!(after_share_count > before_share_count);

        whitenoise
            .update_notifications_enabled(&admin_account, false)
            .await
            .unwrap();

        let after_removal_count =
            wait_for_published_event_count(&whitenoise, &admin_account, after_share_count).await;
        assert!(after_removal_count > after_share_count);
    }

    #[tokio::test]
    async fn test_reconcile_group_push_tokens_prunes_cached_member_mismatch_for_active_leaf() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id = setup_two_member_group_with_welcome_finalization(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;
        let admin_leaf_index = whitenoise
            .create_mdk_for_account(member_account.pubkey)
            .unwrap()
            .group_leaf_map(&group_id)
            .unwrap()
            .iter()
            .find_map(|(leaf_index, pubkey)| {
                (*pubkey == admin_account.pubkey).then_some(*leaf_index)
            })
            .expect("admin leaf should exist in member view");
        let fake_member_pubkey = Keys::generate().public_key();
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();

        GroupPushToken::upsert(
            &member_account.pubkey,
            &group_id,
            &fake_member_pubkey,
            admin_leaf_index,
            &server_pubkey,
            Some(&relay_hint),
            "ciphertext-one",
            &whitenoise.database,
        )
        .await
        .unwrap();

        whitenoise
            .reconcile_group_push_tokens_for_active_leaves(&member_account, &group_id)
            .await
            .unwrap();

        let stored = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(stored.is_empty());
    }

    #[tokio::test]
    async fn test_welcome_finalization_does_not_share_token_for_pending_invite() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let apns_hex_token = "33".repeat(32);

        whitenoise
            .upsert_push_registration(
                &member_account,
                PushPlatform::Apns,
                &apns_hex_token,
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();

        let group_id = setup_two_member_group_with_welcome_finalization(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;

        // The member's group invite is still pending — no token should be shared.
        let cached_tokens = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(
            !cached_tokens
                .iter()
                .any(|token| token.member_pubkey == member_account.pubkey),
            "pending welcome finalization must not share the local push token"
        );
    }

    #[tokio::test]
    async fn test_remove_local_push_token_clears_cache_when_token_removal_publish_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id = setup_two_member_group_with_accepted_account_groups(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();

        whitenoise
            .upsert_push_registration(
                &admin_account,
                PushPlatform::Apns,
                &"66".repeat(32),
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();

        let cached_before_disable = GroupPushToken::find_by_account_and_group(
            &admin_account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(
            cached_before_disable
                .iter()
                .any(|token| token.member_pubkey == admin_account.pubkey),
            "initial share should populate the local cache"
        );

        let relay_swap = NostrGroupDataUpdate {
            name: None,
            description: None,
            image_hash: None,
            image_key: None,
            image_nonce: None,
            image_upload_key: None,
            admins: None,
            relays: Some(vec![
                RelayUrl::parse("ws://localhost:1").unwrap(),
                RelayUrl::parse("ws://localhost:2").unwrap(),
            ]),
            nostr_group_id: None,
        };
        whitenoise
            .update_group_data(&admin_account, &group_id, relay_swap)
            .await
            .unwrap();

        tokio::time::pause();
        let settings = whitenoise
            .update_notifications_enabled(&admin_account, false)
            .await
            .unwrap();
        tokio::time::resume();
        assert!(!settings.notifications_enabled);

        let cached_after_disable = GroupPushToken::find_by_account_and_group(
            &admin_account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(
            cached_after_disable
                .iter()
                .all(|token| token.member_pubkey != admin_account.pubkey),
            "failed removal publishes must still clear the local cache"
        );
    }

    #[tokio::test]
    async fn test_share_and_remove_cover_all_joined_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 2).await;
        let first_member = members[0].0.clone();
        let second_member = members[1].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&first_member, &second_member]).await;

        setup_two_member_group_with_accepted_account_groups(
            &whitenoise,
            &admin_account,
            &first_member,
        )
        .await;
        setup_two_member_group_with_accepted_account_groups(
            &whitenoise,
            &admin_account,
            &second_member,
        )
        .await;

        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let apns_hex_token = "44".repeat(32);

        let before_share_count =
            count_published_events_for_account(&whitenoise, &admin_account).await;

        whitenoise
            .upsert_push_registration(
                &admin_account,
                PushPlatform::Apns,
                &apns_hex_token,
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();

        let expected_share_count = before_share_count + 2;
        wait_for_exact_published_event_count(&whitenoise, &admin_account, expected_share_count)
            .await;

        whitenoise
            .update_notifications_enabled(&admin_account, false)
            .await
            .unwrap();

        let expected_removal_count = expected_share_count + 2;
        wait_for_exact_published_event_count(&whitenoise, &admin_account, expected_removal_count)
            .await;
    }

    #[tokio::test]
    async fn test_upsert_push_registration_removes_shared_token_when_new_registration_is_unshareable()
     {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id = setup_two_member_group_with_accepted_account_groups(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let before_share_count =
            count_published_events_for_account(&whitenoise, &admin_account).await;

        whitenoise
            .upsert_push_registration(
                &admin_account,
                PushPlatform::Apns,
                &"55".repeat(32),
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();

        let after_share_count =
            wait_for_published_event_count(&whitenoise, &admin_account, before_share_count).await;
        assert!(after_share_count > before_share_count);

        let own_leaf_index = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap()
            .own_leaf_index(&group_id)
            .unwrap();
        let cached_after_share = GroupPushToken::find_by_account_and_group(
            &admin_account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(
            cached_after_share
                .iter()
                .any(|token| token.leaf_index == own_leaf_index),
            "initial share should populate the local cache"
        );

        whitenoise
            .upsert_push_registration(
                &admin_account,
                PushPlatform::Fcm,
                "token-without-relay-hint",
                &server_pubkey,
                None,
            )
            .await
            .unwrap();

        let after_removal_count =
            wait_for_published_event_count(&whitenoise, &admin_account, after_share_count).await;
        assert!(after_removal_count > after_share_count);

        let cached_after_removal = GroupPushToken::find_by_account_and_group(
            &admin_account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(
            cached_after_removal
                .iter()
                .all(|token| token.leaf_index != own_leaf_index),
            "transitioning to an unshareable registration should remove the local cache entry"
        );
    }

    #[test]
    fn test_push_registration_debug_redacts_raw_token() {
        let registration = PushRegistration {
            account_pubkey: Keys::generate().public_key(),
            platform: PushPlatform::Apns,
            raw_token: "super-secret-token".to_string(),
            server_pubkey: Keys::generate().public_key(),
            relay_hint: Some(RelayUrl::parse("wss://push.example.com").unwrap()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_shared_at: None,
        };

        let debug_output = format!("{registration:?}");

        assert!(debug_output.contains("<redacted>"));
        assert!(!debug_output.contains("super-secret-token"));
    }

    #[test]
    fn test_group_push_token_debug_redacts_encrypted_token() {
        let token = GroupPushToken {
            account_pubkey: Keys::generate().public_key(),
            mls_group_id: GroupId::from_slice(&[7; 32]),
            member_pubkey: Keys::generate().public_key(),
            leaf_index: 3,
            server_pubkey: Keys::generate().public_key(),
            relay_hint: Some(RelayUrl::parse("wss://push.example.com").unwrap()),
            encrypted_token: "ciphertext-value".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let debug_output = format!("{token:?}");

        assert!(debug_output.contains("<redacted>"));
        assert!(!debug_output.contains("ciphertext-value"));
    }

    #[test]
    fn test_push_platform_from_str_is_case_sensitive() {
        assert_eq!(PushPlatform::from_str("apns").unwrap(), PushPlatform::Apns);
        assert_eq!(PushPlatform::from_str("fcm").unwrap(), PushPlatform::Fcm);
        assert!(PushPlatform::from_str("APNS").is_err());
    }

    #[test]
    fn test_push_registration_push_token_plaintext_rejects_invalid_apns_hex() {
        let registration = PushRegistration {
            account_pubkey: Keys::generate().public_key(),
            platform: PushPlatform::Apns,
            raw_token: "g".repeat(64),
            server_pubkey: Keys::generate().public_key(),
            relay_hint: Some(RelayUrl::parse("wss://push.example.com").unwrap()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_shared_at: None,
        };

        let error = registration.push_token_plaintext().unwrap_err();

        assert!(matches!(
            error,
            WhitenoiseError::InvalidInput(message)
            if message.contains("invalid APNs token hex encoding")
        ));
    }

    #[test]
    fn test_push_registration_push_token_plaintext_rejects_invalid_apns_length() {
        let registration = PushRegistration {
            account_pubkey: Keys::generate().public_key(),
            platform: PushPlatform::Apns,
            raw_token: "too-short".to_string(),
            server_pubkey: Keys::generate().public_key(),
            relay_hint: Some(RelayUrl::parse("wss://push.example.com").unwrap()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_shared_at: None,
        };

        let error = registration.push_token_plaintext().unwrap_err();

        assert!(matches!(
            error,
            WhitenoiseError::InvalidInput(message)
            if message == "APNs token must be 32 raw bytes or 64 hex characters"
        ));
    }

    #[tokio::test]
    async fn test_accept_account_group_shares_local_push_token() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let apns_hex_token = "33".repeat(32);

        whitenoise
            .upsert_push_registration(
                &member_account,
                PushPlatform::Apns,
                &apns_hex_token,
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();

        let group_id = setup_two_member_group_with_welcome_finalization(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;

        // Before acceptance: no token cached for the member.
        let cached_before = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(
            !cached_before
                .iter()
                .any(|token| token.member_pubkey == member_account.pubkey),
            "token must not be cached before acceptance"
        );

        // Accept the group invite.
        whitenoise
            .accept_account_group(&member_account, &group_id)
            .await
            .unwrap();

        // After acceptance: the member's own token should be cached.
        let own_leaf_index = whitenoise
            .create_mdk_for_account(member_account.pubkey)
            .unwrap()
            .own_leaf_index(&group_id)
            .unwrap();
        let cached_after = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(
            cached_after
                .iter()
                .any(|token| token.leaf_index == own_leaf_index),
            "accepting the group should share and cache the local push token"
        );
    }

    #[tokio::test]
    async fn test_joined_group_fanout_only_targets_accepted_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        // Create an accepted group (admin auto-accepts during creation).
        let accepted_group_id = setup_two_member_group_with_accepted_account_groups(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;

        // Create a pending group (welcome finalized but not accepted by member).
        let pending_group_id = setup_two_member_group_with_welcome_finalization(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;

        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let apns_hex_token = "33".repeat(32);

        // Register push and trigger fanout via upsert.
        whitenoise
            .upsert_push_registration(
                &member_account,
                PushPlatform::Apns,
                &apns_hex_token,
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();

        // The accepted group should have the member's token cached.
        let accepted_tokens = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &accepted_group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(
            accepted_tokens
                .iter()
                .any(|token| token.member_pubkey == member_account.pubkey),
            "fanout should share to accepted groups"
        );

        // The pending group must NOT have the member's token cached.
        let pending_tokens = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &pending_group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(
            !pending_tokens
                .iter()
                .any(|token| token.member_pubkey == member_account.pubkey),
            "fanout must not share to pending groups"
        );
    }

    #[tokio::test]
    async fn test_group_creation_shares_creator_push_token() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();

        whitenoise
            .upsert_push_registration(
                &creator,
                PushPlatform::Apns,
                &"44".repeat(32),
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();

        let config = create_nostr_group_config_data(vec![creator.pubkey]);
        let group = whitenoise
            .create_group(
                &creator,
                vec![member_account.pubkey],
                config,
                Some(GroupType::Group),
            )
            .await
            .unwrap();

        // The creator is auto-accepted, so their token should be cached.
        let cached_tokens = GroupPushToken::find_by_account_and_group(
            &creator.pubkey,
            &group.mls_group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(
            cached_tokens
                .iter()
                .any(|token| token.member_pubkey == creator.pubkey),
            "group creation should share the creator's push token"
        );
    }

    #[tokio::test]
    async fn test_decline_account_group_removes_push_token() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();

        whitenoise
            .upsert_push_registration(
                &member_account,
                PushPlatform::Apns,
                &"55".repeat(32),
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();

        // Set up group with accepted account groups so the token gets shared.
        let group_id = setup_two_member_group_with_accepted_account_groups(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;

        // Trigger fanout so the member's token is shared to the accepted group.
        whitenoise
            .share_local_push_token_to_joined_groups(&member_account)
            .await
            .unwrap();

        let cached_before = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(
            cached_before
                .iter()
                .any(|token| token.member_pubkey == member_account.pubkey),
            "token should be cached before decline"
        );

        // Decline the group.
        whitenoise
            .decline_account_group(&member_account, &group_id)
            .await
            .unwrap();

        // The token cache should be cleared.
        let cached_after = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(
            !cached_after
                .iter()
                .any(|token| token.member_pubkey == member_account.pubkey),
            "declining a group should remove any cached push token"
        );
    }
}

//! Push notification registration state and per-group token cache models.

use core::fmt;
use std::time::Duration;

use std::{
    collections::{BTreeMap, HashSet},
    ops::Range,
    str::FromStr,
    sync::Arc,
};

use base64ct::{Base64, Encoding as _};
use chrono::{DateTime, Utc};
use nostr_sdk::{
    Event, EventBuilder, JsonUtil, Keys, Kind, PublicKey, RelayUrl, Tag, TagKind, Timestamp,
    UnsignedEvent, nips::nip44,
};
use serde::{Deserialize, Serialize};

use crate::marmot::GroupId;
use crate::marmot::push::{
    ENCRYPTED_TOKEN_LEN, EncryptedToken, MAX_NOTIFICATION_REQUEST_TOKENS, Mip05Error,
    NOTIFICATION_REQUEST_KIND, NotificationEventBatch, NotificationPlatform, PushTokenPlaintext,
    TOKEN_LIST_RESPONSE_KIND, TOKEN_REMOVAL_KIND, TOKEN_REQUEST_KIND, TokenTag, encrypt_push_token,
    push_token_fingerprint,
};
use crate::whitenoise::{
    Whitenoise,
    accounts::Account,
    database::Database,
    error::{Result, WhitenoiseError},
    event_tracker::EventTracker,
    relays::{Relay, RelayType},
    users::relay_sync,
};
use crate::{
    perf_instrument,
    relay_control::{RelayControlPlane, ephemeral::EphemeralPlane},
};

/// Minimum interval between accepted MIP-05 token messages of the same kind
/// from the same `(account, group_id, sender_leaf_index)` tuple.  Messages
/// that arrive before the cooldown expires are silently dropped.  Normal usage
/// is ≤5 requests per hour, so 30 seconds is generous while blocking spam.
pub(crate) const TOKEN_REQUEST_COOLDOWN: Duration = Duration::from_secs(30);
const MIP05_NOTIFICATION_VERSION: &str = "mip05-v1";
const MIP05_NOTIFICATION_VERSION_TAG: &str = "v";
const NIP59_RANDOM_TIMESTAMP_TWEAK: Range<u64> = 0..172800;

/// Discriminator so token requests and removals have independent cooldowns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TokenRateKind {
    /// Rate-limit bucket for token request events.
    Request,
    /// Rate-limit bucket for token removal events.
    Removal,
}

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

    pub(crate) const fn to_notification_platform(self) -> NotificationPlatform {
        match self {
            Self::Apns => NotificationPlatform::Apns,
            Self::Fcm => NotificationPlatform::Fcm,
        }
    }

    pub(crate) const fn from_notification_platform(platform: NotificationPlatform) -> Self {
        match platform {
            NotificationPlatform::Apns => Self::Apns,
            NotificationPlatform::Fcm => Self::Fcm,
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
    pub platform: Option<PushPlatform>,
    pub token_fingerprint: Option<String>,
    pub server_pubkey: PublicKey,
    pub relay_hint: Option<RelayUrl>,
    pub encrypted_token: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Safe push-token cache summary for UI debugging.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GroupPushDebugInfo {
    pub total_token_count: usize,
    pub active_token_count: usize,
    pub stale_token_count: usize,
    pub missing_relay_hint_count: usize,
    pub last_token_list_updated_at: Option<DateTime<Utc>>,
    pub local_registration: LocalPushRegistrationDebugInfo,
    pub tokens: Vec<GroupPushTokenDebugEntry>,
}

/// Redacted local registration status for group push-token debugging.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LocalPushRegistrationDebugInfo {
    pub registered: bool,
    pub shareable: bool,
    pub notifications_enabled: bool,
    pub local_leaf_index: Option<u32>,
    pub local_token_cached: bool,
}

/// Redacted cached group-token status for one MLS leaf.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GroupPushTokenDebugEntry {
    pub member_pubkey: PublicKey,
    pub leaf_index: u32,
    pub server_pubkey: PublicKey,
    pub has_relay_hint: bool,
    pub active_leaf: bool,
    pub member_matches_active_leaf: bool,
    pub is_local_member: bool,
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
            .field("platform", &self.platform)
            .field("token_fingerprint", &self.token_fingerprint)
            .field("server_pubkey", &self.server_pubkey)
            .field("relay_hint", &self.relay_hint)
            .field("encrypted_token", &"<redacted>")
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// Maximum number of concurrently-active delayed MIP-05 token-list response tasks.
/// Tasks that cannot acquire a permit are dropped; the requester can retry via a
/// subsequent token request event.
pub(crate) const MAX_CONCURRENT_TOKEN_RESPONSE_TASKS: usize = 10;

const PUSH_GROUP_MESSAGE_KINDS: [u16; 3] = [
    TOKEN_REQUEST_KIND,
    TOKEN_LIST_RESPONSE_KIND,
    TOKEN_REMOVAL_KIND,
];

pub(crate) fn is_push_group_message_kind(kind: Kind) -> bool {
    PUSH_GROUP_MESSAGE_KINDS.contains(&kind.as_u16())
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
            platform: token.platform.map(PushPlatform::to_notification_platform),
            token_fingerprint: token.token_fingerprint.clone(),
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
    relay_control: &RelayControlPlane,
    event_tracker: &Arc<dyn EventTracker>,
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

    let synced_relays = match relay_sync::sync_relay_type_for_pubkey(
        database,
        relay_control,
        event_tracker,
        server_pubkey,
        RelayType::Inbox,
        &deduped_hints,
    )
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
    relay_control: &RelayControlPlane,
    event_tracker: &Arc<dyn EventTracker>,
    ephemeral: &EphemeralPlane,
    account_pubkey: PublicKey,
    token_counts_by_server: &BTreeMap<PublicKey, usize>,
    batches: Vec<NotificationEventBatch>,
) {
    for batch in batches {
        let token_count = token_counts_by_server
            .get(&batch.server_pubkey)
            .copied()
            .unwrap_or_default();
        let event_count = batch.events.len();
        let relay_resolution = resolve_notification_server_relays(
            database,
            relay_control,
            event_tracker,
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

fn build_notification_batches(
    tokens: Vec<TokenTag>,
) -> std::result::Result<Vec<NotificationEventBatch>, Mip05Error> {
    if tokens.is_empty() {
        return Err(Mip05Error::NotificationRequestMustIncludeToken);
    }

    group_tokens_by_server(tokens)
        .into_iter()
        .map(|(server_pubkey, server_tokens)| {
            build_notification_batch_for_server(server_pubkey, server_tokens)
        })
        .collect()
}

fn group_tokens_by_server(tokens: Vec<TokenTag>) -> BTreeMap<PublicKey, Vec<TokenTag>> {
    let mut grouped_tokens: BTreeMap<PublicKey, Vec<TokenTag>> = BTreeMap::new();

    for token in tokens {
        grouped_tokens
            .entry(token.server_pubkey)
            .or_default()
            .push(token);
    }

    grouped_tokens
}

fn build_notification_batch_for_server(
    server_pubkey: PublicKey,
    server_tokens: Vec<TokenTag>,
) -> std::result::Result<NotificationEventBatch, Mip05Error> {
    validate_unique_encrypted_tokens(&server_tokens)?;
    let relay_hints = collect_relay_hints(&server_tokens);
    let events = server_tokens
        .chunks(MAX_NOTIFICATION_REQUEST_TOKENS)
        .map(|chunk| {
            let encrypted_tokens = chunk
                .iter()
                .map(|token| token.encrypted_token.clone())
                .collect::<Vec<_>>();
            build_notification_event_chunk(&server_pubkey, encrypted_tokens)
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(NotificationEventBatch {
        server_pubkey,
        relay_hints,
        events,
    })
}

fn validate_unique_encrypted_tokens(tokens: &[TokenTag]) -> std::result::Result<(), Mip05Error> {
    let mut unique_tokens = HashSet::with_capacity(tokens.len());

    for token in tokens {
        if !unique_tokens.insert(token.encrypted_token.clone()) {
            return Err(Mip05Error::DuplicateEncryptedToken);
        }
    }

    Ok(())
}

fn collect_relay_hints(tokens: &[TokenTag]) -> Vec<RelayUrl> {
    let mut relay_hints = Vec::new();

    for token in tokens {
        if !relay_hints.contains(&token.relay_hint) {
            relay_hints.push(token.relay_hint.clone());
        }
    }

    relay_hints
}

fn build_notification_event_chunk(
    server_pubkey: &PublicKey,
    encrypted_tokens: Vec<EncryptedToken>,
) -> std::result::Result<Event, Mip05Error> {
    let sender_keys = Keys::generate();
    let rumor = build_notification_request_rumor(
        sender_keys.public_key(),
        Timestamp::now(),
        encrypted_tokens,
    )?;
    let seal = build_notification_request_seal(&sender_keys, server_pubkey, rumor)?;
    EventBuilder::gift_wrap_from_seal(server_pubkey, &seal, []).map_err(|error| {
        tracing::warn!(
            target: "whitenoise::push_notifications",
            error = %error,
            "Failed to gift-wrap notification request"
        );
        Mip05Error::NotificationRequestGiftWrapFailed
    })
}

fn build_notification_request_rumor(
    pubkey: PublicKey,
    created_at: Timestamp,
    tokens: Vec<EncryptedToken>,
) -> std::result::Result<UnsignedEvent, Mip05Error> {
    if tokens.is_empty() {
        return Err(Mip05Error::NotificationRequestMustIncludeToken);
    }

    let mut content_bytes = Vec::with_capacity(tokens.len() * ENCRYPTED_TOKEN_LEN);
    for token in &tokens {
        content_bytes.extend_from_slice(token.as_bytes());
    }

    let mut rumor = UnsignedEvent::new(
        pubkey,
        created_at,
        Kind::from(NOTIFICATION_REQUEST_KIND),
        [Tag::custom(
            TagKind::Custom(MIP05_NOTIFICATION_VERSION_TAG.into()),
            [MIP05_NOTIFICATION_VERSION],
        )],
        Base64::encode_string(&content_bytes),
    );
    rumor.ensure_id();
    Ok(rumor)
}

fn build_notification_request_seal(
    sender_keys: &Keys,
    server_pubkey: &PublicKey,
    rumor: UnsignedEvent,
) -> std::result::Result<Event, Mip05Error> {
    let content = nip44::encrypt(
        sender_keys.secret_key(),
        server_pubkey,
        rumor.as_json(),
        nip44::Version::default(),
    )
    .map_err(|error| {
        tracing::warn!(
            target: "whitenoise::push_notifications",
            error = %error,
            "Failed to encrypt notification request"
        );
        Mip05Error::NotificationRequestEncryptionFailed
    })?;

    EventBuilder::new(Kind::Seal, content)
        .custom_created_at(Timestamp::tweaked(NIP59_RANDOM_TIMESTAMP_TWEAK))
        .sign_with_keys(sender_keys)
        .map_err(|error| {
            tracing::warn!(
                target: "whitenoise::push_notifications",
                error = %error,
                "Failed to sign notification request seal"
            );
            Mip05Error::NotificationRequestSealFailed
        })
}

pub(crate) async fn publish_notification_requests_after_delivery_from_cache(
    database: &Database,
    relay_control: &RelayControlPlane,
    event_tracker: &Arc<dyn EventTracker>,
    ephemeral: &EphemeralPlane,
    account_pubkey: PublicKey,
    cached_tokens: &[GroupPushToken],
    active_leaf_map: &BTreeMap<u32, PublicKey>,
    sender_member_pubkey: &PublicKey,
) -> Result<()> {
    let recipient_tokens = prepare_notification_recipient_tokens(
        &account_pubkey,
        cached_tokens,
        active_leaf_map,
        sender_member_pubkey,
    );

    if recipient_tokens.is_empty() {
        return Ok(());
    }

    let token_counts_by_server = notification_token_counts_by_server(&recipient_tokens);
    let batches = build_notification_batches(recipient_tokens)?;

    publish_notification_batches_best_effort(
        database,
        relay_control,
        event_tracker,
        ephemeral,
        account_pubkey,
        &token_counts_by_server,
        batches,
    )
    .await;

    Ok(())
}

impl Whitenoise {
    #[perf_instrument("push_notifications")]
    async fn local_push_token_tag(&self, account: &Account) -> Result<Option<TokenTag>> {
        let session = self.require_session(&account.pubkey)?;
        let Some(registration) = session.push().registration().await? else {
            return Ok(None);
        };

        registration.token_tag()
    }
}

pub(crate) fn validate_raw_token(raw_token: &str) -> Result<()> {
    if raw_token.trim().is_empty() {
        return Err(WhitenoiseError::InvalidInput(
            "push registration token must not be empty".to_string(),
        ));
    }

    Ok(())
}

impl PushRegistration {
    pub(crate) fn token_tag(&self) -> Result<Option<TokenTag>> {
        let plaintext = self.push_token_plaintext()?;
        let Some(relay_hint) = self.relay_hint.clone() else {
            return Ok(None);
        };

        let encrypted_token = encrypt_push_token(&self.server_pubkey, &plaintext)?;

        Ok(Some(TokenTag {
            encrypted_token,
            platform: Some(plaintext.platform()),
            token_fingerprint: Some(push_token_fingerprint(
                plaintext.platform(),
                plaintext.device_token(),
            )),
            server_pubkey: self.server_pubkey,
            relay_hint,
        }))
    }

    pub(crate) fn push_token_plaintext(&self) -> Result<PushTokenPlaintext> {
        match self.platform {
            PushPlatform::Apns => {
                // Apple push notification tokens are variable-length opaque data.
                // The app layer surfaces them as hex-encoded strings, so decode
                // from hex and pass the raw bytes through.
                let token_bytes = hex::decode(&self.raw_token).map_err(|error| {
                    WhitenoiseError::InvalidInput(format!(
                        "invalid APNs token hex encoding: {error}"
                    ))
                })?;

                if token_bytes.is_empty() {
                    return Err(WhitenoiseError::InvalidInput(
                        "APNs token must not be empty".to_string(),
                    ));
                }

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
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use cgka_traits::transport::TransportMessage;
    use nostr_sdk::{EventId, Filter, Keys, Kind, RelayUrl, Tag, nips::nip59};
    use transport_nostr_peeler::NostrTransportEvent;

    use super::*;
    use crate::marmot::publish::{MarmotMessagePublisher, MarmotPublishOutcome};
    use crate::relay_control::{
        ephemeral::{EphemeralPlane, EphemeralPlaneConfig},
        sessions::{RelaySessionAuthPolicy, RelaySessionReconnectPolicy},
    };
    use crate::whitenoise::group_information::GroupType;
    use crate::whitenoise::test_utils::{
        ObsoleteMlsArtifacts, assert_obsolete_mls_artifacts_absent,
        count_published_events_for_account, create_group_config, create_mock_whitenoise,
        setup_multiple_test_accounts, wait_for_exact_published_event_count,
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

    fn obsolete_mls_artifacts_for_session(
        session: &crate::whitenoise::session::AccountSession,
    ) -> ObsoleteMlsArtifacts {
        ObsoleteMlsArtifacts {
            storage_path: crate::whitenoise::test_utils::obsolete_mls_storage_path(
                &session.account_pubkey,
                &session.shared.config.data_dir,
            ),
        }
    }

    fn assert_obsolete_mls_artifacts_absent_for_session(
        session: &crate::whitenoise::session::AccountSession,
    ) {
        let artifacts = obsolete_mls_artifacts_for_session(session);
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    async fn create_projected_two_member_group(
        whitenoise: &Whitenoise,
        admin_account: &Account,
        member_account: &Account,
    ) -> GroupId {
        whitenoise
            .require_session(&admin_account.pubkey)
            .unwrap()
            .groups()
            .create_group(
                vec![member_account.pubkey],
                create_group_config(vec![admin_account.pubkey]),
                None,
            )
            .await
            .unwrap()
            .mls_group_id
    }

    #[derive(Default)]
    struct CapturingMarmotPublisher {
        messages: Arc<Mutex<Vec<TransportMessage>>>,
    }

    impl CapturingMarmotPublisher {
        fn messages(&self) -> Vec<TransportMessage> {
            self.messages.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl MarmotMessagePublisher for CapturingMarmotPublisher {
        async fn publish(&self, message: TransportMessage) -> MarmotPublishOutcome {
            self.messages.lock().unwrap().push(message);
            MarmotPublishOutcome::Published { accepted_count: 1 }
        }
    }

    async fn create_pending_projected_two_member_group(
        whitenoise: &Whitenoise,
        admin_account: &Account,
        member_account: &Account,
    ) -> GroupId {
        let publisher = CapturingMarmotPublisher::default();
        let created_group = whitenoise
            .require_session(&admin_account.pubkey)
            .unwrap()
            .groups()
            .create_marmot_group_with_publisher(
                vec![member_account.pubkey],
                create_group_config(vec![admin_account.pubkey]),
                None,
                &publisher,
            )
            .await
            .unwrap();

        let welcome_message = publisher
            .messages()
            .into_iter()
            .find(|message| {
                matches!(
                    message.envelope,
                    cgka_traits::TransportEnvelope::Welcome { .. }
                )
            })
            .expect("Darkmatter group creation should publish a welcome");
        let giftwrap_event = NostrTransportEvent::from_transport_message(&welcome_message)
            .unwrap()
            .to_verified_nostr_event()
            .unwrap();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();

        whitenoise
            .handle_giftwrap(&member_session, member_account, giftwrap_event)
            .await
            .unwrap();

        created_group.mls_group_id
    }

    async fn create_accepted_projected_two_member_group(
        whitenoise: &Whitenoise,
        admin_account: &Account,
        member_account: &Account,
    ) -> GroupId {
        let group_id =
            create_pending_projected_two_member_group(whitenoise, admin_account, member_account)
                .await;

        whitenoise
            .require_session(&member_account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .accept()
            .await
            .unwrap();

        group_id
    }

    async fn darkmatter_push_member_index_map(
        whitenoise: &Whitenoise,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
    ) -> BTreeMap<u32, PublicKey> {
        let session = whitenoise.require_session(account_pubkey).unwrap();
        let marmot = session
            .marmot
            .as_ref()
            .expect("test account should have a Marmot session")
            .clone();
        let marmot_group_id = cgka_traits::types::GroupId::new(group_id.as_slice().to_vec());

        marmot
            .lock()
            .await
            .push_member_index_map(&marmot_group_id)
            .unwrap()
    }

    fn member_push_index(active_member_index: &BTreeMap<u32, PublicKey>, pubkey: PublicKey) -> u32 {
        active_member_index
            .iter()
            .find_map(|(leaf_index, member_pubkey)| {
                (*member_pubkey == pubkey).then_some(*leaf_index)
            })
            .expect("member should have a Darkmatter push index")
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
            platform: None,
            token_fingerprint: None,
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
        let session = whitenoise.require_session(&account.pubkey)?;
        crate::whitenoise::session::push::PushResponseContext::from_session(&session)
            .publish_notification_requests_after_delivery(group_id)
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
                    .shared
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
                    .shared
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
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        message_id: &EventId,
    ) {
        let session = whitenoise.session(account_pubkey).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let message = AggregatedMessage::find_by_id(
                    &message_id.to_string(),
                    group_id,
                    account_pubkey,
                    &session.account_db.inner,
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
            whitenoise.shared.database.clone(),
            whitenoise.event_sender.clone(),
            whitenoise.shared.relay_control.observability().clone(),
            whitenoise.shared.event_tracker.clone(),
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
    async fn test_get_group_push_debug_info_returns_count_and_latest_update_time() {
        let account_keys = Keys::generate();
        let session = crate::whitenoise::session::test_helpers::test_session_with_marmot_keys(
            account_keys.clone(),
        )
        .await;
        let group_id = GroupId::from_slice(&[42; 32]);
        let server_pubkey = Keys::generate().public_key();
        let member_one_pubkey = Keys::generate().public_key();
        let member_two_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();

        let first_token = session
            .repos
            .group_push_tokens
            .upsert(
                &group_id,
                &member_one_pubkey,
                1,
                &server_pubkey,
                Some(&relay_hint),
                "ciphertext-one",
            )
            .await
            .unwrap();
        let second_token = session
            .repos
            .group_push_tokens
            .upsert(
                &group_id,
                &member_two_pubkey,
                2,
                &server_pubkey,
                Some(&relay_hint),
                "ciphertext-two",
            )
            .await
            .unwrap();

        let debug_info = session
            .push()
            .get_group_debug_info(&group_id)
            .await
            .unwrap();

        assert_eq!(debug_info.total_token_count, 2);
        assert_eq!(debug_info.active_token_count, 0);
        assert_eq!(debug_info.stale_token_count, 2);
        assert_eq!(debug_info.missing_relay_hint_count, 0);
        assert_eq!(
            debug_info.last_token_list_updated_at,
            [first_token.updated_at, second_token.updated_at]
                .into_iter()
                .max()
        );
        assert_eq!(
            debug_info.local_registration,
            LocalPushRegistrationDebugInfo {
                registered: false,
                shareable: false,
                notifications_enabled: true,
                local_leaf_index: None,
                local_token_cached: false,
            }
        );
        assert_obsolete_mls_artifacts_absent_for_session(&session);
        assert_eq!(debug_info.tokens.len(), 2);
        assert!(debug_info.tokens.iter().all(|token| !token.active_leaf));
        assert!(
            debug_info
                .tokens
                .iter()
                .all(|token| !token.member_matches_active_leaf)
        );

        let serialized = serde_json::to_string(&debug_info).unwrap();
        assert!(
            !serialized.contains("ciphertext-one") && !serialized.contains("ciphertext-two"),
            "debug info must stay redacted when serialized"
        );
    }

    #[tokio::test]
    async fn test_resolve_notification_server_relays_prefers_cached_inbox_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let server_pubkey = Keys::generate().public_key();
        let relay_url = RelayUrl::parse("wss://cached-push.example.com").unwrap();
        let (server_user, _created) = crate::whitenoise::users::User::find_or_create_by_pubkey(
            &server_pubkey,
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        let relay = Relay::find_or_create_by_url(&relay_url, &whitenoise.shared.database)
            .await
            .unwrap();

        server_user
            .add_relay(&relay, RelayType::Inbox, &whitenoise.shared.database)
            .await
            .unwrap();

        let (resolved_relays, relay_source) = resolve_notification_server_relays(
            &whitenoise.shared.database,
            &whitenoise.shared.relay_control,
            &whitenoise.shared.event_tracker,
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
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        let relay = Relay::find_or_create_by_url(&cached_relay, &whitenoise.shared.database)
            .await
            .unwrap();

        server_user
            .add_relay(&relay, RelayType::Inbox, &whitenoise.shared.database)
            .await
            .unwrap();

        let (resolved_relays, relay_source) = resolve_notification_server_relays(
            &whitenoise.shared.database,
            &whitenoise.shared.relay_control,
            &whitenoise.shared.event_tracker,
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
                    .relays(RelayType::Inbox, &whitenoise.shared.database)
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
            &whitenoise.shared.database,
            &whitenoise.shared.relay_control,
            &whitenoise.shared.event_tracker,
            server_pubkey,
            &[],
        )
        .await
        .unwrap();

        assert!(resolved_relays.is_empty());
        assert_eq!(relay_source, NotificationRelaySource::HintFallback);

        let server_user = crate::whitenoise::users::User::find_by_pubkey(
            &server_pubkey,
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        assert!(
            server_user
                .relays(RelayType::Inbox, &whitenoise.shared.database)
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
            platform: None,
            token_fingerprint: None,
            server_pubkey,
            relay_hint: RelayUrl::parse("wss://ignored-hint.example.com").unwrap(),
        }];
        let mut batches = build_notification_batches(token_tags.clone()).unwrap();

        batches[0].relay_hints.clear();

        publish_notification_batches_best_effort(
            &whitenoise.shared.database,
            &whitenoise.shared.relay_control,
            &whitenoise.shared.event_tracker,
            &test_ephemeral_plane(&whitenoise),
            sender_pubkey,
            &notification_token_counts_by_server(&token_tags),
            batches,
        )
        .await;

        let server_user = crate::whitenoise::users::User::find_by_pubkey(
            &server_pubkey,
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        assert!(
            server_user
                .relays(RelayType::Inbox, &whitenoise.shared.database)
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

        let group_id =
            create_projected_two_member_group(&whitenoise, &admin_account, &member_account).await;
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
            &whitenoise
                .require_session(&admin_account.pubkey)
                .unwrap()
                .account_db
                .inner
                .pool,
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

        let group_id =
            create_projected_two_member_group(&whitenoise, &admin_account, &member_account).await;
        let server_inbox_relays = server_account
            .inbox_relays(&whitenoise.shared)
            .await
            .unwrap();
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
            .remove_all_relays(&whitenoise.shared.database)
            .await
            .unwrap();

        let active_member_index =
            darkmatter_push_member_index_map(&whitenoise, &admin_account.pubkey, &group_id).await;
        let admin_leaf_index = member_push_index(&active_member_index, admin_account.pubkey);
        let member_leaf_index = member_push_index(&active_member_index, member_account.pubkey);
        let sender_token = encrypted_fcm_token_base64(&server_account.pubkey, "sender-own-device");
        let recipient_token = encrypt_push_token(
            &server_account.pubkey,
            &PushTokenPlaintext::new(NotificationPlatform::Fcm, b"recipient-device".to_vec())
                .unwrap(),
        )
        .unwrap();

        let admin_pool = &whitenoise
            .require_session(&admin_account.pubkey)
            .unwrap()
            .account_db
            .inner
            .pool;
        GroupPushToken::upsert(
            &admin_account.pubkey,
            &group_id,
            &admin_account.pubkey,
            admin_leaf_index,
            &server_account.pubkey,
            Some(&server_relay_urls[0]),
            &sender_token,
            admin_pool,
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
            admin_pool,
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
        let tokens = notification_request_tokens_for_test(&unwrapped.rumor);

        assert_eq!(tokens, vec![recipient_token]);
    }

    #[tokio::test]
    async fn test_reaction_delivery_publishes_expected_446_tokens() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let mut accounts = setup_multiple_test_accounts(&whitenoise, 2).await;
        let (member_account, _member_keys) = accounts.remove(0);
        let (server_account, server_keys) = accounts.remove(0);

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id =
            create_projected_two_member_group(&whitenoise, &admin_account, &member_account).await;
        let server_inbox_relays = server_account
            .inbox_relays(&whitenoise.shared)
            .await
            .unwrap();
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
            .remove_all_relays(&whitenoise.shared.database)
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
        wait_for_message_sent_status(
            &whitenoise,
            &admin_account.pubkey,
            &group_id,
            &chat_result.message.id,
        )
        .await;

        let active_member_index =
            darkmatter_push_member_index_map(&whitenoise, &admin_account.pubkey, &group_id).await;
        let admin_leaf_index = member_push_index(&active_member_index, admin_account.pubkey);
        let member_leaf_index = member_push_index(&active_member_index, member_account.pubkey);
        let sender_token = encrypted_fcm_token_base64(&server_account.pubkey, "sender-own-device");
        let recipient_token = encrypt_push_token(
            &server_account.pubkey,
            &PushTokenPlaintext::new(NotificationPlatform::Fcm, b"recipient-device".to_vec())
                .unwrap(),
        )
        .unwrap();

        let admin_pool = &whitenoise
            .require_session(&admin_account.pubkey)
            .unwrap()
            .account_db
            .inner
            .pool;
        GroupPushToken::upsert(
            &admin_account.pubkey,
            &group_id,
            &admin_account.pubkey,
            admin_leaf_index,
            &server_account.pubkey,
            Some(&server_relay_urls[0]),
            &sender_token,
            admin_pool,
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
            admin_pool,
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
        let tokens = notification_request_tokens_for_test(&unwrapped.rumor);

        assert_eq!(tokens, vec![recipient_token]);
    }

    #[tokio::test]
    async fn test_publish_notification_batches_best_effort_publishes_multiple_1059_events_for_large_batch()
     {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let sender_account = whitenoise.create_identity().await.unwrap();
        let mut server_accounts = setup_multiple_test_accounts(&whitenoise, 1).await;
        let (server_account, server_keys) = server_accounts.remove(0);
        let server_inbox_relays = server_account
            .inbox_relays(&whitenoise.shared)
            .await
            .unwrap();
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
            .remove_all_relays(&whitenoise.shared.database)
            .await
            .unwrap();

        let relay_hint = server_relay_urls[0].clone();
        let token_tags: Vec<TokenTag> = (0..(MAX_NOTIFICATION_REQUEST_TOKENS + 1))
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
                platform: None,
                token_fingerprint: None,
                server_pubkey: server_account.pubkey,
                relay_hint: relay_hint.clone(),
            })
            .collect();
        let batches = build_notification_batches(token_tags.clone()).unwrap();

        assert_eq!(batches.len(), 1);
        assert!(batches[0].events.len() > 1);

        publish_notification_batches_best_effort(
            &whitenoise.shared.database,
            &whitenoise.shared.relay_control,
            &whitenoise.shared.event_tracker,
            &whitenoise.shared.relay_control.ephemeral(),
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
            received_tokens.extend(notification_request_tokens_for_test(&unwrapped.rumor));
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

        let group_id = create_projected_two_member_group(&whitenoise, &creator, &member).await;
        let send_result = whitenoise
            .send_message_to_group(&creator, &group_id, "status isolation".to_string(), 9, None)
            .await
            .unwrap();

        wait_for_message_sent_status(
            &whitenoise,
            &creator.pubkey,
            &group_id,
            &send_result.message.id,
        )
        .await;

        let active_member_index =
            darkmatter_push_member_index_map(&whitenoise, &creator.pubkey, &group_id).await;
        let member_leaf_index = member_push_index(&active_member_index, member.pubkey);
        let unreachable_server_pubkey = Keys::generate().public_key();
        let unreachable_relay = RelayUrl::parse("ws://127.0.0.1:1").unwrap();

        let creator_pool = &whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .account_db
            .inner
            .pool;
        GroupPushToken::upsert(
            &creator.pubkey,
            &group_id,
            &member.pubkey,
            member_leaf_index,
            &unreachable_server_pubkey,
            Some(&unreachable_relay),
            &encrypted_fcm_token_base64(&unreachable_server_pubkey, "bad-recipient-device"),
            creator_pool,
        )
        .await
        .unwrap();
        let ephemeral = test_ephemeral_plane(&whitenoise);

        let cached_tokens =
            GroupPushToken::find_by_account_and_group(&creator.pubkey, &group_id, creator_pool)
                .await
                .unwrap();

        publish_notification_requests_after_delivery_from_cache(
            &whitenoise.shared.database,
            &whitenoise.shared.relay_control,
            &whitenoise.shared.event_tracker,
            &ephemeral,
            creator.pubkey,
            &cached_tokens,
            &active_member_index,
            &creator.pubkey,
        )
        .await
        .unwrap();

        let creator_session = whitenoise.session(&creator.pubkey).unwrap();
        let cached_message = AggregatedMessage::find_by_id(
            &send_result.message.id.to_string(),
            &group_id,
            &creator.pubkey,
            &creator_session.account_db.inner,
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
                .session(&account.pubkey)
                .unwrap()
                .push()
                .registration()
                .await
                .unwrap()
                .is_none()
        );

        let created = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .push()
            .upsert_registration(
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
            .session(&account.pubkey)
            .unwrap()
            .push()
            .upsert_registration(PushPlatform::Fcm, "token-two", &server_pubkey, None)
            .await
            .unwrap();
        assert_eq!(replaced.platform, PushPlatform::Fcm);
        assert_eq!(replaced.raw_token, "token-two");
        assert_eq!(replaced.relay_hint, None);

        let stored = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .push()
            .registration()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored, replaced);

        whitenoise
            .session(&account.pubkey)
            .unwrap()
            .push()
            .clear_registration()
            .await
            .unwrap();
        assert!(
            whitenoise
                .session(&account.pubkey)
                .unwrap()
                .push()
                .registration()
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
            .session(&account.pubkey)
            .unwrap()
            .push()
            .upsert_registration(PushPlatform::Fcm, "device-token", &server_pubkey, None)
            .await
            .unwrap();

        let settings = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .settings()
            .update_notifications_enabled(false)
            .await
            .unwrap();
        assert!(!settings.notifications_enabled);

        let stored = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .push()
            .registration()
            .await
            .unwrap();
        assert!(stored.is_some());
        assert_eq!(stored.unwrap().raw_token, "device-token");
    }

    #[tokio::test]
    async fn test_upsert_push_registration_rejects_blank_token() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let server_pubkey = Keys::generate().public_key();

        let err = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .push()
            .upsert_registration(PushPlatform::Apns, "   ", &server_pubkey, None)
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

        let group_id =
            create_projected_two_member_group(&whitenoise, &admin_account, &member_account).await;
        let before_count = count_published_events_for_account(&whitenoise, &admin_account).await;
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let apns_hex_token = "11".repeat(32);

        whitenoise
            .session(&admin_account.pubkey)
            .unwrap()
            .push()
            .upsert_registration(
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

        let active_member_index =
            darkmatter_push_member_index_map(&whitenoise, &admin_account.pubkey, &group_id).await;
        let own_leaf_index = member_push_index(&active_member_index, admin_account.pubkey);
        let cached_tokens = GroupPushToken::find_by_account_and_group(
            &admin_account.pubkey,
            &group_id,
            &whitenoise
                .require_session(&admin_account.pubkey)
                .unwrap()
                .account_db
                .inner
                .pool,
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

        create_projected_two_member_group(&whitenoise, &admin_account, &member_account).await;
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let apns_hex_token = "22".repeat(32);

        let before_share_count =
            count_published_events_for_account(&whitenoise, &admin_account).await;

        whitenoise
            .session(&admin_account.pubkey)
            .unwrap()
            .push()
            .upsert_registration(
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
            .require_session(&admin_account.pubkey)
            .unwrap()
            .settings()
            .update_notifications_enabled(false)
            .await
            .unwrap();

        let after_removal_count =
            wait_for_published_event_count(&whitenoise, &admin_account, after_share_count).await;
        assert!(after_removal_count > after_share_count);
    }

    #[tokio::test]
    async fn test_clear_push_registration_publishes_token_removal() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id =
            create_projected_two_member_group(&whitenoise, &admin_account, &member_account).await;
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();

        let before_share_count =
            count_published_events_for_account(&whitenoise, &admin_account).await;
        whitenoise
            .session(&admin_account.pubkey)
            .unwrap()
            .push()
            .upsert_registration(
                PushPlatform::Apns,
                &"66".repeat(32),
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();
        let after_share_count =
            wait_for_published_event_count(&whitenoise, &admin_account, before_share_count).await;

        whitenoise
            .session(&admin_account.pubkey)
            .unwrap()
            .push()
            .clear_registration()
            .await
            .unwrap();

        let after_clear_count =
            wait_for_published_event_count(&whitenoise, &admin_account, after_share_count).await;
        assert!(after_clear_count > after_share_count);
        assert!(
            whitenoise
                .session(&admin_account.pubkey)
                .unwrap()
                .push()
                .registration()
                .await
                .unwrap()
                .is_none()
        );

        let active_member_index =
            darkmatter_push_member_index_map(&whitenoise, &admin_account.pubkey, &group_id).await;
        let own_leaf_index = member_push_index(&active_member_index, admin_account.pubkey);
        let admin_pool = &whitenoise
            .require_session(&admin_account.pubkey)
            .unwrap()
            .account_db
            .inner
            .pool;
        let cached_after_clear =
            GroupPushToken::find_by_account_and_group(&admin_account.pubkey, &group_id, admin_pool)
                .await
                .unwrap();
        assert!(
            cached_after_clear
                .iter()
                .all(|token| token.leaf_index != own_leaf_index),
            "clearing a registration should remove the local cached push token"
        );
    }

    #[tokio::test]
    async fn test_reconcile_group_push_tokens_prunes_cached_member_mismatch_for_active_leaf() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id =
            create_pending_projected_two_member_group(&whitenoise, &admin_account, &member_account)
                .await;
        let active_member_index =
            darkmatter_push_member_index_map(&whitenoise, &member_account.pubkey, &group_id).await;
        let admin_leaf_index = member_push_index(&active_member_index, admin_account.pubkey);
        let fake_member_pubkey = Keys::generate().public_key();
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();

        let member_pool = &whitenoise
            .require_session(&member_account.pubkey)
            .unwrap()
            .account_db
            .inner
            .pool;
        GroupPushToken::upsert(
            &member_account.pubkey,
            &group_id,
            &fake_member_pubkey,
            admin_leaf_index,
            &server_pubkey,
            Some(&relay_hint),
            "ciphertext-one",
            member_pool,
        )
        .await
        .unwrap();

        whitenoise
            .session(&member_account.pubkey)
            .unwrap()
            .push()
            .reconcile_group_tokens_for_active_leaves(&group_id)
            .await
            .unwrap();

        let stored = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            member_pool,
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
            .session(&member_account.pubkey)
            .unwrap()
            .push()
            .upsert_registration(
                PushPlatform::Apns,
                &apns_hex_token,
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();

        let group_id =
            create_pending_projected_two_member_group(&whitenoise, &admin_account, &member_account)
                .await;

        // The member's group invite is still pending — no token should be shared.
        let cached_tokens = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            &whitenoise
                .require_session(&member_account.pubkey)
                .unwrap()
                .account_db
                .inner
                .pool,
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
    async fn test_share_and_remove_cover_all_joined_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 2).await;
        let first_member = members[0].0.clone();
        let second_member = members[1].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&first_member, &second_member]).await;

        create_projected_two_member_group(&whitenoise, &admin_account, &first_member).await;
        create_projected_two_member_group(&whitenoise, &admin_account, &second_member).await;

        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let apns_hex_token = "44".repeat(32);

        let before_share_count =
            count_published_events_for_account(&whitenoise, &admin_account).await;

        whitenoise
            .session(&admin_account.pubkey)
            .unwrap()
            .push()
            .upsert_registration(
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
            .require_session(&admin_account.pubkey)
            .unwrap()
            .settings()
            .update_notifications_enabled(false)
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

        let group_id =
            create_projected_two_member_group(&whitenoise, &admin_account, &member_account).await;
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let before_share_count =
            count_published_events_for_account(&whitenoise, &admin_account).await;

        whitenoise
            .session(&admin_account.pubkey)
            .unwrap()
            .push()
            .upsert_registration(
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

        let active_member_index =
            darkmatter_push_member_index_map(&whitenoise, &admin_account.pubkey, &group_id).await;
        let own_leaf_index = member_push_index(&active_member_index, admin_account.pubkey);
        let admin_pool = &whitenoise
            .require_session(&admin_account.pubkey)
            .unwrap()
            .account_db
            .inner
            .pool;
        let cached_after_share =
            GroupPushToken::find_by_account_and_group(&admin_account.pubkey, &group_id, admin_pool)
                .await
                .unwrap();
        assert!(
            cached_after_share
                .iter()
                .any(|token| token.leaf_index == own_leaf_index),
            "initial share should populate the local cache"
        );

        whitenoise
            .session(&admin_account.pubkey)
            .unwrap()
            .push()
            .upsert_registration(
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

        let cached_after_removal =
            GroupPushToken::find_by_account_and_group(&admin_account.pubkey, &group_id, admin_pool)
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
            platform: None,
            token_fingerprint: None,
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
    fn test_push_registration_push_token_plaintext_rejects_non_hex_apns_token() {
        let registration = PushRegistration {
            account_pubkey: Keys::generate().public_key(),
            platform: PushPlatform::Apns,
            raw_token: "not-valid-hex!!".to_string(),
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
    fn test_push_registration_push_token_plaintext_rejects_empty_apns_token() {
        let registration = PushRegistration {
            account_pubkey: Keys::generate().public_key(),
            platform: PushPlatform::Apns,
            raw_token: String::new(),
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
            if message.contains("must not be empty")
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
            .session(&member_account.pubkey)
            .unwrap()
            .push()
            .upsert_registration(
                PushPlatform::Apns,
                &apns_hex_token,
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();

        let group_id =
            create_pending_projected_two_member_group(&whitenoise, &admin_account, &member_account)
                .await;

        // Before acceptance: no token cached for the member.
        let member_pool = &whitenoise
            .require_session(&member_account.pubkey)
            .unwrap()
            .account_db
            .inner
            .pool;
        let cached_before = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            member_pool,
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
            .require_session(&member_account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .accept()
            .await
            .unwrap();

        // After acceptance: the member's own token should be cached.
        let active_member_index =
            darkmatter_push_member_index_map(&whitenoise, &member_account.pubkey, &group_id).await;
        let own_leaf_index = member_push_index(&active_member_index, member_account.pubkey);
        let cached_after = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            member_pool,
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
        let accepted_group_id = create_accepted_projected_two_member_group(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;

        // Create a pending group (welcome finalized but not accepted by member).
        let pending_group_id =
            create_pending_projected_two_member_group(&whitenoise, &admin_account, &member_account)
                .await;

        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let apns_hex_token = "33".repeat(32);

        // Register push and trigger fanout via upsert.
        whitenoise
            .session(&member_account.pubkey)
            .unwrap()
            .push()
            .upsert_registration(
                PushPlatform::Apns,
                &apns_hex_token,
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();

        // The accepted group should have the member's token cached.
        let member_pool = &whitenoise
            .require_session(&member_account.pubkey)
            .unwrap()
            .account_db
            .inner
            .pool;
        let accepted_tokens = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &accepted_group_id,
            member_pool,
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
            member_pool,
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
            .session(&creator.pubkey)
            .unwrap()
            .push()
            .upsert_registration(
                PushPlatform::Apns,
                &"44".repeat(32),
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();

        let config = create_group_config(vec![creator.pubkey]);
        let group = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member_account.pubkey], config, Some(GroupType::Group))
            .await
            .unwrap();

        // The creator is auto-accepted, so their token should be cached.
        let cached_tokens = GroupPushToken::find_by_account_and_group(
            &creator.pubkey,
            &group.mls_group_id,
            &whitenoise
                .require_session(&creator.pubkey)
                .unwrap()
                .account_db
                .inner
                .pool,
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
            .session(&member_account.pubkey)
            .unwrap()
            .push()
            .upsert_registration(
                PushPlatform::Apns,
                &"55".repeat(32),
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();

        // Set up group with accepted account groups so the token gets shared.
        let group_id = create_accepted_projected_two_member_group(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;

        // Trigger fanout so the member's token is shared to the accepted group.
        whitenoise
            .require_session(&member_account.pubkey)
            .unwrap()
            .push()
            .share_local_token_to_joined_groups()
            .await
            .unwrap();

        let member_pool = &whitenoise
            .require_session(&member_account.pubkey)
            .unwrap()
            .account_db
            .inner
            .pool;
        let cached_before = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            member_pool,
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
            .require_session(&member_account.pubkey)
            .unwrap()
            .membership()
            .for_group(&group_id)
            .decline()
            .await
            .unwrap();

        // The token cache should be cleared.
        let cached_after = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            member_pool,
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

    fn assert_notification_rumor_only_carries_version_tag(rumor: &nostr_sdk::UnsignedEvent) {
        assert_eq!(rumor.kind, Kind::Custom(446));
        let tags: Vec<Vec<String>> = rumor
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect();
        assert_eq!(tags, vec![vec!["v".to_string(), "mip05-v1".to_string()]]);
    }

    fn notification_request_tokens_for_test(
        rumor: &nostr_sdk::UnsignedEvent,
    ) -> Vec<EncryptedToken> {
        assert_notification_rumor_only_carries_version_tag(rumor);
        let content = Base64::decode_vec(&rumor.content).unwrap();
        assert!(!content.is_empty());
        assert_eq!(content.len() % ENCRYPTED_TOKEN_LEN, 0);
        content
            .chunks_exact(ENCRYPTED_TOKEN_LEN)
            .map(|chunk| EncryptedToken::from_slice(chunk).unwrap())
            .collect()
    }
}

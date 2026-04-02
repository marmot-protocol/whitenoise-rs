//! Push notification registration state and per-group token cache models.

use core::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use ::rand::Rng;
use futures::stream::{self, StreamExt};
use chrono::{DateTime, Utc};
use mdk_core::mip05::{
    EncryptedToken, LeafTokenTag, Mip05GroupMessage, NotificationPlatform, PushTokenPlaintext,
    TokenTag, build_token_list_response_rumor, build_token_removal_rumor,
    build_token_request_rumor, encrypt_push_token, parse_group_message,
};
use mdk_core::prelude::{GroupId, group_types::GroupState};
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr_sdk::{EventId, Kind, PublicKey, RelayUrl};
use serde::{Deserialize, Serialize};

use crate::whitenoise::{
    Whitenoise, WhitenoiseConfig,
    account_settings::AccountSettings,
    accounts::Account,
    database::Database,
    error::{Result, WhitenoiseError},
};
use crate::{perf_instrument, relay_control::RelayControlPlane};

/// Maximum number of groups to process concurrently for push token operations.
/// This bounds the parallelism to avoid overwhelming the relay connections
/// while still reducing latency from O(N × RTT) to roughly O(N / CONCURRENCY + RTT).
const MAX_CONCURRENT_GROUP_OPERATIONS: usize = 5;

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
    let event = mdk.create_message(group_id, rumor)?;

    relay_control
        .publish_event_to(event, &account.pubkey, &relay_urls)
        .await?;
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
    /// This only updates local persistence. MLS token sharing and notification
    /// request publication are handled in later MIP-05 PRs.
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

    #[perf_instrument("push_notifications")]
    async fn share_push_token_to_joined_groups(
        &self,
        account: &Account,
        token_tag: &TokenTag,
    ) -> Result<()> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        let groups = mdk.get_groups()?;

        // Process groups concurrently to reduce latency from O(N × RTT) to
        // roughly O(ceil(N / CONCURRENCY) × RTT). Uses buffer_unordered to
        // limit concurrent relay operations and prevent connection overload.
        let results: Vec<Result<()>> = stream::iter(groups)
            .filter(|group| futures::future::ready(group.state == GroupState::Active))
            .map(|group| async move {
                let rumor = build_token_request_rumor(
                    account.pubkey,
                    nostr_sdk::Timestamp::now(),
                    vec![token_tag.clone()],
                )?;
                self.publish_push_group_message(account, &group.mls_group_id, rumor)
                    .await?;
                self.sync_local_group_push_token_cache(account, &group.mls_group_id, Some(token_tag))
                    .await
            })
            .buffer_unordered(MAX_CONCURRENT_GROUP_OPERATIONS)
            .collect()
            .await;

        let publish_failures: Vec<String> = results
            .into_iter()
            .filter_map(|result| match result {
                Ok(()) => None,
                Err(e) => Some(e.to_string()),
            })
            .collect();

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

        // Process groups concurrently to reduce latency from O(N × RTT) to
        // roughly O(ceil(N / CONCURRENCY) × RTT).
        let results: Vec<Result<()>> = stream::iter(groups)
            .filter(|group| futures::future::ready(group.state == GroupState::Active))
            .map(|group| async move {
                let rumor = build_token_removal_rumor(account.pubkey, nostr_sdk::Timestamp::now());
                self.publish_push_group_message(account, &group.mls_group_id, rumor)
                    .await?;
                self.sync_local_group_push_token_cache(account, &group.mls_group_id, None)
                    .await
            })
            .buffer_unordered(MAX_CONCURRENT_GROUP_OPERATIONS)
            .collect()
            .await;

        let publish_failures: Vec<String> = results
            .into_iter()
            .filter_map(|result| match result {
                Ok(()) => None,
                Err(e) => Some(e.to_string()),
            })
            .collect();

        if publish_failures.is_empty() {
            Ok(())
        } else {
            Err(WhitenoiseError::Configuration(format!(
                "failed to remove push token from one or more groups: {}",
                publish_failures.join(", ")
            )))
        }
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
    use mdk_core::prelude::NostrGroupDataUpdate;
    use nostr_sdk::{Keys, RelayUrl};

    use super::*;
    use crate::whitenoise::test_utils::{
        count_published_events_for_account, create_mock_whitenoise, setup_multiple_test_accounts,
        setup_two_member_group_with_welcome_finalization, wait_for_exact_published_event_count,
        wait_for_key_package_publication, wait_for_published_event_count,
    };

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

        let group_id = setup_two_member_group_with_welcome_finalization(
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

        setup_two_member_group_with_welcome_finalization(
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
    async fn test_welcome_flow_shares_existing_registration_for_new_member() {
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

        let before_count = count_published_events_for_account(&whitenoise, &member_account).await;
        let group_id = setup_two_member_group_with_welcome_finalization(
            &whitenoise,
            &admin_account,
            &member_account,
        )
        .await;
        let after_count =
            wait_for_published_event_count(&whitenoise, &member_account, before_count).await;
        assert!(after_count > before_count);

        let own_leaf_index = whitenoise
            .create_mdk_for_account(member_account.pubkey)
            .unwrap()
            .own_leaf_index(&group_id)
            .unwrap();
        let cached_tokens = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(
            cached_tokens
                .iter()
                .any(|token| token.leaf_index == own_leaf_index),
            "welcome-triggered share should update the local cache for the new group"
        );
    }

    #[tokio::test]
    async fn test_remove_local_push_token_clears_cache_when_token_removal_publish_fails() {
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

        setup_two_member_group_with_welcome_finalization(
            &whitenoise,
            &admin_account,
            &first_member,
        )
        .await;
        setup_two_member_group_with_welcome_finalization(
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

        let group_id = setup_two_member_group_with_welcome_finalization(
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
}

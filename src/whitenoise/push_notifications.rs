//! Push notification registration state and per-group token cache models.

use core::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use ::rand::Rng;
use chrono::{DateTime, Utc};
use mdk_core::mip05::{
    EncryptedToken, LeafTokenTag, Mip05GroupMessage, NotificationPlatform, PushTokenPlaintext,
    TokenTag, build_token_list_response_rumor, build_token_removal_rumor,
    build_token_request_rumor, encrypt_push_token, parse_group_message,
};
use mdk_core::prelude::{GroupId, group_types::GroupState};
use nostr_sdk::{EventId, Kind, PublicKey, RelayUrl};
use serde::{Deserialize, Serialize};

use crate::perf_instrument;
use crate::whitenoise::{
    Whitenoise,
    account_settings::AccountSettings,
    accounts::Account,
    error::{Result, WhitenoiseError},
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
            .field("leaf_index", &self.leaf_index)
            .field("server_pubkey", &self.server_pubkey)
            .field("relay_hint", &self.relay_hint)
            .field("encrypted_token", &"<redacted>")
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
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

        let registration = PushRegistration::upsert(
            &account.pubkey,
            platform,
            raw_token,
            server_pubkey,
            relay_hint,
            &self.database,
        )
        .await?;

        if let Err(error) = self.share_local_push_token_to_joined_groups(account).await {
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
        if !Self::is_push_group_message_kind(message.kind) {
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

                self.merge_token_request(account, &message.mls_group_id, leaf_index, request)
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

        let pending_responses = Arc::clone(&self.pending_push_token_responses);
        let delay_ms = ::rand::rng().random_range(1_000..=3_000);

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;

            if !pending_responses.contains_key(&(
                account.pubkey,
                group_id.clone(),
                request_event_id,
            )) {
                return;
            }

            let whitenoise = match Self::get_instance() {
                Ok(whitenoise) => whitenoise,
                Err(error) => {
                    tracing::warn!(
                        target: "whitenoise::push_notifications",
                        account = %account.pubkey.to_hex(),
                        group_id = %hex::encode(group_id.as_slice()),
                        request_event_id = %request_event_id.to_hex(),
                        error = %error,
                        "Skipping delayed MIP-05 token-list response because Whitenoise is unavailable"
                    );
                    return;
                }
            };

            if let Err(error) = whitenoise
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

        let mdk = self.create_mdk_for_account(account.pubkey)?;
        let groups = mdk.get_groups()?;
        let mut publish_failures = Vec::new();

        for group in groups {
            if group.state != GroupState::Active {
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
            if active_leaf_map.contains_key(&token.leaf_index) {
                continue;
            }

            GroupPushToken::delete(&account.pubkey, group_id, token.leaf_index, &self.database)
                .await?;
        }

        Ok(())
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

        let rumor = build_token_request_rumor(
            account.pubkey,
            nostr_sdk::Timestamp::now(),
            vec![token_tag],
        )?;
        self.publish_push_group_message(account, group_id, rumor)
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

    fn is_push_group_message_kind(kind: Kind) -> bool {
        kind == Kind::from(mdk_core::mip05::TOKEN_REQUEST_KIND)
            || kind == Kind::from(mdk_core::mip05::TOKEN_LIST_RESPONSE_KIND)
            || kind == Kind::from(mdk_core::mip05::TOKEN_REMOVAL_KIND)
    }

    #[perf_instrument("push_notifications")]
    async fn merge_token_request(
        &self,
        account: &Account,
        mls_group_id: &GroupId,
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
        for token in response.tokens {
            GroupPushToken::upsert(
                &account.pubkey,
                mls_group_id,
                token.leaf_index,
                &token.token_tag.server_pubkey,
                Some(&token.token_tag.relay_hint),
                &token.token_tag.encrypted_token.to_base64(),
                &self.database,
            )
            .await?;
        }

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
        let tokens =
            GroupPushToken::find_by_account_and_group(&account.pubkey, group_id, &self.database)
                .await?;

        let mut response_tokens = Vec::with_capacity(tokens.len());
        for token in tokens {
            let relay_hint = token.relay_hint.clone().ok_or_else(|| {
                WhitenoiseError::InvalidEvent(
                    "group push token missing relay hint for token-list response".to_string(),
                )
            })?;

            response_tokens.push(LeafTokenTag {
                leaf_index: token.leaf_index,
                token_tag: TokenTag {
                    encrypted_token: EncryptedToken::from_base64(&token.encrypted_token)?,
                    server_pubkey: token.server_pubkey,
                    relay_hint,
                },
            });
        }

        Ok(response_tokens)
    }

    #[perf_instrument("push_notifications")]
    async fn local_push_token_tag(&self, account: &Account) -> Result<Option<TokenTag>> {
        let Some(registration) = self.push_registration(account).await? else {
            return Ok(None);
        };

        let plaintext = registration.push_token_plaintext()?;
        let encrypted_token = encrypt_push_token(&registration.server_pubkey, &plaintext)?;
        let relay_hint = registration.relay_hint.clone().ok_or_else(|| {
            WhitenoiseError::InvalidInput(
                "push registration relay hint is required before sharing tokens".to_string(),
            )
        })?;

        Ok(Some(TokenTag {
            encrypted_token,
            server_pubkey: registration.server_pubkey,
            relay_hint,
        }))
    }

    #[perf_instrument("push_notifications")]
    async fn publish_push_group_message(
        &self,
        account: &Account,
        group_id: &GroupId,
        rumor: nostr_sdk::UnsignedEvent,
    ) -> Result<()> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        let relay_urls = Self::ensure_group_relays(&mdk, group_id)?;
        let event = mdk.create_message(group_id, rumor)?;

        self.publish_event_with_retry(event, &account.pubkey, &relay_urls)
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
    fn push_token_plaintext(&self) -> Result<PushTokenPlaintext> {
        match self.platform {
            PushPlatform::Apns => {
                let token_bytes = if self.raw_token.len() == 64 {
                    hex::decode(&self.raw_token).map_err(|error| {
                        WhitenoiseError::InvalidInput(format!(
                            "invalid APNs token hex encoding: {error}"
                        ))
                    })?
                } else {
                    self.raw_token.as_bytes().to_vec()
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
    use nostr_sdk::{EventBuilder, Keys, RelayUrl};

    use super::*;
    use crate::whitenoise::{
        relays::Relay,
        test_utils::{
            create_mock_whitenoise, create_nostr_group_config_data, setup_multiple_test_accounts,
            wait_for_key_package_publication,
        },
    };

    async fn setup_two_member_group(
        whitenoise: &Whitenoise,
        admin_account: &Account,
        member_account: &Account,
    ) -> GroupId {
        let relay_urls = Relay::urls(&member_account.key_package_relays(whitenoise).await.unwrap());
        let key_pkg_event = whitenoise
            .relay_control
            .fetch_user_key_package(member_account.pubkey, &relay_urls)
            .await
            .unwrap()
            .expect("member must have a published key package");

        let admin_mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();
        let create_result = admin_mdk
            .create_group(
                &admin_account.pubkey,
                vec![key_pkg_event],
                create_nostr_group_config_data(vec![admin_account.pubkey]),
            )
            .unwrap();

        let group_id = create_result.group.mls_group_id.clone();
        let welcome_rumor = create_result
            .welcome_rumors
            .first()
            .expect("welcome rumor exists")
            .clone();

        let admin_signer = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&admin_account.pubkey)
            .unwrap();
        let giftwrap =
            EventBuilder::gift_wrap(&admin_signer, &member_account.pubkey, welcome_rumor, vec![])
                .await
                .unwrap();

        whitenoise
            .handle_giftwrap(member_account, giftwrap)
            .await
            .expect("member should process welcome successfully");

        group_id
    }

    async fn setup_group_with_welcome_for_member(
        whitenoise: &Whitenoise,
        admin_account: &Account,
        member_account: &Account,
    ) -> GroupId {
        let relay_urls = Relay::urls(&member_account.key_package_relays(whitenoise).await.unwrap());
        let key_pkg_event = whitenoise
            .relay_control
            .fetch_user_key_package(member_account.pubkey, &relay_urls)
            .await
            .unwrap()
            .expect("member must have a published key package");

        let admin_mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();
        let create_result = admin_mdk
            .create_group(
                &admin_account.pubkey,
                vec![key_pkg_event],
                create_nostr_group_config_data(vec![admin_account.pubkey]),
            )
            .unwrap();

        let group_id = create_result.group.mls_group_id.clone();
        let welcome_rumor = create_result
            .welcome_rumors
            .first()
            .expect("welcome rumor exists")
            .clone();

        let admin_signer = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&admin_account.pubkey)
            .unwrap();
        let giftwrap =
            EventBuilder::gift_wrap(&admin_signer, &member_account.pubkey, welcome_rumor, vec![])
                .await
                .unwrap();

        whitenoise
            .handle_giftwrap(member_account, giftwrap)
            .await
            .expect("member should process welcome successfully");

        group_id
    }

    async fn wait_for_published_event_count(
        whitenoise: &Whitenoise,
        account: &Account,
        previous_count: usize,
    ) -> usize {
        for _ in 0..20 {
            let new_count = count_published_events_for_account(whitenoise, account).await;
            if new_count > previous_count {
                return new_count;
            }

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        panic!("timed out waiting for new published event");
    }

    async fn count_published_events_for_account(
        whitenoise: &Whitenoise,
        account: &Account,
    ) -> usize {
        let account_id = account.id.expect("account must be persisted");
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM published_events WHERE account_id = ?")
                .bind(account_id)
                .fetch_one(&whitenoise.database.pool)
                .await
                .unwrap();

        usize::try_from(count.0).unwrap()
    }

    async fn wait_for_exact_published_event_count(
        whitenoise: &Whitenoise,
        account: &Account,
        expected_count: usize,
    ) {
        for _ in 0..20 {
            let count = count_published_events_for_account(whitenoise, account).await;
            if count == expected_count {
                return;
            }

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        panic!("timed out waiting for published event count {expected_count}");
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
                "token-one",
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();
        assert_eq!(created.platform, PushPlatform::Apns);
        assert_eq!(created.raw_token, "token-one");
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
                PushPlatform::Apns,
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

        setup_two_member_group(&whitenoise, &admin_account, &member_account).await;
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
    }

    #[tokio::test]
    async fn test_disabling_notifications_publishes_token_removal() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        setup_two_member_group(&whitenoise, &admin_account, &member_account).await;
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

        let group_id =
            setup_group_with_welcome_for_member(&whitenoise, &admin_account, &member_account).await;
        let before_count = count_published_events_for_account(&whitenoise, &member_account).await;
        let member_mdk = whitenoise
            .create_mdk_for_account(member_account.pubkey)
            .unwrap();
        let group_name = member_mdk
            .get_group(&group_id)
            .unwrap()
            .expect("member should have group after welcome")
            .name;

        Whitenoise::finalize_welcome_with_instance(
            &whitenoise,
            &member_account,
            &group_id,
            &group_name,
            EventId::all_zeros(),
            admin_account.pubkey,
        )
        .await;

        let after_count =
            wait_for_published_event_count(&whitenoise, &member_account, before_count).await;
        assert!(after_count > before_count);
    }

    #[tokio::test]
    async fn test_share_and_remove_cover_all_joined_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 2).await;
        let first_member = members[0].0.clone();
        let second_member = members[1].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&first_member, &second_member]).await;

        setup_two_member_group(&whitenoise, &admin_account, &first_member).await;
        setup_two_member_group(&whitenoise, &admin_account, &second_member).await;

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

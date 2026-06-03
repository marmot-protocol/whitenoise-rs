//! Push notification operations scoped to a single account session.
//!
//! `PushOps` is a borrow-based view that provides push registration management,
//! per-group token sharing/removal, MIP-05 message handling, and token
//! reconciliation — all without threading `account_pubkey` through every call.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use crate::marmot::{GroupId, GroupState};
use cgka_traits::{GroupId as MarmotGroupId, StorageError};
use futures::stream::{self, StreamExt};
use nostr_sdk::{EventId, PublicKey, RelayUrl};
use tokio::sync::Mutex;

use super::AccountSession;
use crate::marmot::Message;
use crate::marmot::message::app_payload_from_unsigned_event;
use crate::marmot::publish::{MarmotPublishOutcome, publish_effects};
use crate::marmot::push::{
    MemberTokenTag, Mip05GroupMessage, TokenListResponse, TokenRequest, TokenTag,
    build_token_list_response_app_event, build_token_removal_app_event,
    build_token_update_app_event, parse_group_message_rumor, push_token_fingerprint,
};
use crate::marmot::session::MarmotSession;
use crate::marmot::storage::WhitenoiseMarmotStorage;
use crate::marmot::transport::{MarmotPublishRoutes, MarmotRelayControlPublisher};
use crate::perf_instrument;
use crate::whitenoise::accounts_groups::AccountGroup;
use crate::whitenoise::database::Database;
use crate::whitenoise::database::account::AccountRepositories;
use crate::whitenoise::database::group_push_tokens::GroupPushTokenUpsert;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::event_tracker::EventTracker;
use crate::whitenoise::push_notifications::{
    GroupPushDebugInfo, GroupPushTokenDebugEntry, LocalPushRegistrationDebugInfo, PushPlatform,
    PushRegistration, TOKEN_REQUEST_COOLDOWN, TokenRateKind, is_push_group_message_kind,
    publish_notification_requests_after_delivery_from_cache, validate_raw_token,
};

/// Maximum number of groups to publish push token events to concurrently.
const MAX_CONCURRENT_GROUP_PUBLISHES: usize = 10;

pub(crate) struct PushResponseContext {
    account_pubkey: PublicKey,
    marmot: Option<Arc<Mutex<MarmotSession>>>,
    marmot_storage: WhitenoiseMarmotStorage,
    repos: AccountRepositories,
    database: Arc<Database>,
    event_tracker: Arc<dyn EventTracker>,
    ephemeral: super::relay_handles::AccountEphemeralHandle,
    relay_control: Arc<crate::relay_control::RelayControlPlane>,
}

impl PushResponseContext {
    pub(crate) fn from_session(session: &AccountSession) -> Self {
        Self {
            account_pubkey: session.account_pubkey,
            marmot: session.marmot.clone(),
            marmot_storage: session.marmot_storage.clone(),
            repos: session.repos.clone(),
            database: Arc::clone(&session.shared.database),
            event_tracker: Arc::clone(&session.shared.event_tracker),
            ephemeral: session.ephemeral.clone(),
            relay_control: Arc::clone(session.group_handle.relay_control()),
        }
    }

    pub(crate) async fn respond_to_token_request(
        &self,
        group_id: &GroupId,
        _request_event_id: EventId,
    ) -> Result<()> {
        if !self.has_darkmatter_projection(group_id)? {
            return Ok(());
        }

        if let Some(event) = self
            .build_darkmatter_token_list_response_event(group_id)
            .await?
        {
            self.publish_darkmatter_push_event(group_id, event).await?;
        }

        Ok(())
    }

    pub(crate) async fn publish_notification_requests_after_delivery(
        &self,
        group_id: &GroupId,
    ) -> Result<()> {
        let cached_tokens = self.repos.group_push_tokens.find_by_group(group_id).await?;
        if cached_tokens.is_empty() {
            return Ok(());
        }

        let active_leaf_map = self.active_push_member_index_map(group_id).await?;
        let sender_member_pubkey = active_leaf_map
            .iter()
            .find_map(|(_, pubkey)| (*pubkey == self.account_pubkey).then_some(*pubkey))
            .ok_or_else(|| {
                WhitenoiseError::Configuration(
                    "sender member missing from active group member index".to_string(),
                )
            })?;
        let ephemeral = self.ephemeral.clone_inner();

        publish_notification_requests_after_delivery_from_cache(
            self.database.as_ref(),
            self.relay_control.as_ref(),
            &self.event_tracker,
            &ephemeral,
            self.account_pubkey,
            &cached_tokens,
            &active_leaf_map,
            &sender_member_pubkey,
        )
        .await
    }

    async fn active_push_member_index_map(
        &self,
        group_id: &GroupId,
    ) -> Result<BTreeMap<u32, PublicKey>> {
        let has_darkmatter_projection = self.has_darkmatter_projection(group_id)?;
        if let Some(marmot_session) = &self.marmot {
            let marmot_group_id = MarmotGroupId::new(group_id.as_slice().to_vec());
            let marmot = marmot_session.lock().await;
            match marmot.push_member_index_map(&marmot_group_id) {
                Ok(member_index) => return Ok(member_index),
                Err(error) if is_missing_marmot_group_error(&error) => {
                    if has_darkmatter_projection {
                        return Err(WhitenoiseError::MarmotSessionUnavailable(
                            self.account_pubkey,
                        ));
                    }
                    return Err(WhitenoiseError::GroupNotFound);
                }
                Err(error) => return Err(error),
            }
        } else if has_darkmatter_projection {
            return Err(WhitenoiseError::MarmotSessionUnavailable(
                self.account_pubkey,
            ));
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    fn has_darkmatter_projection(&self, group_id: &GroupId) -> Result<bool> {
        let marmot_group_id = MarmotGroupId::new(group_id.as_slice().to_vec());
        Ok(self
            .marmot_storage
            .find_group_projection(&marmot_group_id)?
            .is_some())
    }

    #[perf_instrument("push_notifications")]
    async fn build_darkmatter_token_list_response_event(
        &self,
        group_id: &GroupId,
    ) -> Result<Option<nostr_sdk::UnsignedEvent>> {
        let active_leaf_map = self.active_push_member_index_map(group_id).await?;
        let cached_tokens = self.repos.group_push_tokens.find_by_group(group_id).await?;
        let mut response_tokens = Vec::new();

        for token in cached_tokens {
            let Some(active_member_pubkey) = active_leaf_map.get(&token.leaf_index) else {
                continue;
            };
            if active_member_pubkey != &token.member_pubkey {
                tracing::warn!(
                    target: "whitenoise::push_notifications",
                    account = %self.account_pubkey.to_hex(),
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
                    account = %self.account_pubkey.to_hex(),
                    group_id = %hex::encode(group_id.as_slice()),
                    member_pubkey = %token.member_pubkey.to_hex(),
                    "Skipping cached push token without relay hint in Darkmatter token-list response"
                );
                continue;
            };
            let Some(platform) = token.platform.map(PushPlatform::to_notification_platform) else {
                tracing::warn!(
                    target: "whitenoise::push_notifications",
                    account = %self.account_pubkey.to_hex(),
                    group_id = %hex::encode(group_id.as_slice()),
                    member_pubkey = %token.member_pubkey.to_hex(),
                    "Skipping cached push token without platform metadata in Darkmatter token-list response"
                );
                continue;
            };
            let Some(token_fingerprint) = token.token_fingerprint.clone() else {
                tracing::warn!(
                    target: "whitenoise::push_notifications",
                    account = %self.account_pubkey.to_hex(),
                    group_id = %hex::encode(group_id.as_slice()),
                    member_pubkey = %token.member_pubkey.to_hex(),
                    "Skipping cached push token without fingerprint metadata in Darkmatter token-list response"
                );
                continue;
            };
            let encrypted_token = match crate::marmot::push::EncryptedToken::from_base64(
                &token.encrypted_token,
            ) {
                Ok(encrypted_token) => encrypted_token,
                Err(error) => {
                    tracing::warn!(
                        target: "whitenoise::push_notifications",
                        account = %self.account_pubkey.to_hex(),
                        group_id = %hex::encode(group_id.as_slice()),
                        member_pubkey = %token.member_pubkey.to_hex(),
                        error = %error,
                        "Skipping cached push token with invalid encrypted payload in Darkmatter token-list response"
                    );
                    continue;
                }
            };

            response_tokens.push(MemberTokenTag {
                member_pubkey: token.member_pubkey,
                leaf_index: token.leaf_index,
                token_tag: TokenTag {
                    encrypted_token,
                    platform: Some(platform),
                    token_fingerprint: Some(token_fingerprint),
                    server_pubkey: token.server_pubkey,
                    relay_hint,
                },
            });
        }

        if response_tokens.is_empty() {
            return Ok(None);
        }

        Ok(Some(build_token_list_response_app_event(
            self.account_pubkey,
            nostr_sdk::Timestamp::now(),
            response_tokens,
        )?))
    }

    async fn publish_darkmatter_push_event(
        &self,
        group_id: &GroupId,
        event: nostr_sdk::UnsignedEvent,
    ) -> Result<()> {
        let has_darkmatter_projection = self.has_darkmatter_projection(group_id)?;
        let Some(marmot_session) = self.marmot.clone() else {
            if has_darkmatter_projection {
                return Err(WhitenoiseError::MarmotSessionUnavailable(
                    self.account_pubkey,
                ));
            }
            return Err(WhitenoiseError::GroupNotFound);
        };

        let marmot_group_id = MarmotGroupId::new(group_id.as_slice().to_vec());
        let event_id = event.id.ok_or_else(|| {
            WhitenoiseError::Internal(
                "Darkmatter push app event is missing its Nostr id".to_string(),
            )
        })?;
        let payload = app_payload_from_unsigned_event(&event, event_id)?;
        let (effects, group_route) = {
            let mut session = marmot_session.lock().await;
            let group_route = match session.group_publish_route(&marmot_group_id) {
                Ok(group_route) => group_route,
                Err(error) if is_missing_marmot_group_error(&error) => {
                    if has_darkmatter_projection {
                        return Err(WhitenoiseError::MarmotSessionUnavailable(
                            self.account_pubkey,
                        ));
                    }
                    return Err(WhitenoiseError::GroupNotFound);
                }
                Err(error) => return Err(error),
            };
            let effects = session.send_app_message(marmot_group_id, payload).await?;
            (effects, group_route)
        };

        let routes = MarmotPublishRoutes::new().with_group_publish_route(group_route);
        let publisher = MarmotRelayControlPublisher::new(&self.ephemeral, routes);
        let published = publish_effects(marmot_session, &publisher, effects).await?;

        if let Some(failure) = published.failures.first() {
            return Err(WhitenoiseError::MarmotPublishFailed(failure.reason.clone()));
        }
        if !published.queued.is_empty() {
            return Err(WhitenoiseError::MarmotPublishFailed(
                "Darkmatter push app-message publish was queued; queued push publish is not implemented"
                    .to_string(),
            ));
        }
        if !published
            .reports
            .iter()
            .any(|report| matches!(report.outcome, MarmotPublishOutcome::Published { .. }))
        {
            return Err(WhitenoiseError::Internal(
                "Darkmatter push app-message send produced no published transport message"
                    .to_string(),
            ));
        }

        Ok(())
    }
}

/// Account-scoped push notification operations.
pub struct PushOps<'a> {
    session: &'a AccountSession,
}

impl<'a> PushOps<'a> {
    pub(super) fn new(session: &'a AccountSession) -> Self {
        Self { session }
    }

    // ── Public API ────────────────────────────────────────────────────

    /// Returns the locally stored push registration for this account, if present.
    #[perf_instrument("push_notifications")]
    pub async fn registration(&self) -> Result<Option<PushRegistration>> {
        self.session.repos.push_registrations.find().await
    }

    /// Creates or replaces the locally stored push registration for this account.
    ///
    /// Updates local persistence and, when notifications remain shareable,
    /// best-effort MIP-05 token sharing for the account's joined groups.
    #[perf_instrument("push_notifications")]
    pub async fn upsert_registration(
        &self,
        platform: PushPlatform,
        raw_token: &str,
        server_pubkey: &PublicKey,
        relay_hint: Option<&RelayUrl>,
    ) -> Result<PushRegistration> {
        validate_raw_token(raw_token)?;
        let pending_registration = PushRegistration {
            account_pubkey: self.session.account_pubkey,
            platform,
            raw_token: raw_token.to_string(),
            server_pubkey: *server_pubkey,
            relay_hint: relay_hint.cloned(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            last_shared_at: None,
        };
        pending_registration.push_token_plaintext()?;

        let previous_registration = self.registration().await?;
        let previous_token_tag = previous_registration
            .as_ref()
            .map(PushRegistration::token_tag)
            .transpose()?
            .flatten();
        let new_token_tag = pending_registration.token_tag()?;

        let registration = self
            .session
            .repos
            .push_registrations
            .upsert(platform, raw_token, server_pubkey, relay_hint)
            .await?;

        let notifications_enabled = self.session.repos.settings.notifications_enabled().await?;

        if previous_token_tag.is_some() && new_token_tag.is_none() {
            if let Err(error) = self
                .remove_registration_token_from_joined_groups(previous_registration)
                .await
            {
                tracing::warn!(
                    target: "whitenoise::push_notifications",
                    account = %self.session.account_pubkey.to_hex(),
                    error = %error,
                    "Failed to remove previously shared push token after registration became unshareable"
                );
            }
        } else if notifications_enabled
            && let Some(new_token_tag) = new_token_tag.as_ref()
            && let Err(error) = self
                .share_token_to_joined_groups(&registration, new_token_tag)
                .await
        {
            tracing::warn!(
                target: "whitenoise::push_notifications",
                account = %self.session.account_pubkey.to_hex(),
                error = %error,
                "Failed to share updated push registration to joined groups"
            );
        }

        Ok(registration)
    }

    /// Removes the locally stored push registration for this account.
    #[perf_instrument("push_notifications")]
    pub async fn clear_registration(&self) -> Result<()> {
        let previous_registration = self.registration().await?;
        self.session.repos.push_registrations.delete().await?;

        if let Err(error) = self
            .remove_registration_token_from_joined_groups(previous_registration)
            .await
        {
            tracing::warn!(
                target: "whitenoise::push_notifications",
                account = %self.session.account_pubkey.to_hex(),
                error = %error,
                "Failed to remove shared push token from joined groups after local clear"
            );
        }

        Ok(())
    }

    /// Returns a non-sensitive summary of cached push-token state for a group.
    #[perf_instrument("push_notifications")]
    pub async fn get_group_debug_info(&self, group_id: &GroupId) -> Result<GroupPushDebugInfo> {
        let cached_tokens = self
            .session
            .repos
            .group_push_tokens
            .find_by_group(group_id)
            .await?;
        let active_leaf_map = match self.active_push_member_index_map(group_id).await {
            Ok(member_index) => member_index,
            Err(WhitenoiseError::GroupNotFound) => BTreeMap::new(),
            Err(error) => return Err(error),
        };
        let local_leaf_index = self.local_push_member_index(&active_leaf_map);
        let notifications_enabled = self.session.repos.settings.notifications_enabled().await?;
        let registration = self.registration().await?;
        let shareable = registration
            .as_ref()
            .map(PushRegistration::token_tag)
            .transpose()?
            .flatten()
            .is_some();
        let local_token_cached = local_leaf_index.is_some_and(|leaf_index| {
            cached_tokens.iter().any(|token| {
                token.leaf_index == leaf_index && token.member_pubkey == self.session.account_pubkey
            })
        });
        let last_token_list_updated_at = cached_tokens.iter().map(|token| token.updated_at).max();

        let tokens: Vec<GroupPushTokenDebugEntry> = cached_tokens
            .iter()
            .map(|token| {
                let active_member_pubkey = active_leaf_map.get(&token.leaf_index);
                GroupPushTokenDebugEntry {
                    member_pubkey: token.member_pubkey,
                    leaf_index: token.leaf_index,
                    server_pubkey: token.server_pubkey,
                    has_relay_hint: token.relay_hint.is_some(),
                    active_leaf: active_member_pubkey.is_some(),
                    member_matches_active_leaf: active_member_pubkey
                        .is_some_and(|pubkey| pubkey == &token.member_pubkey),
                    is_local_member: token.member_pubkey == self.session.account_pubkey,
                    updated_at: token.updated_at,
                }
            })
            .collect();
        let active_token_count = tokens
            .iter()
            .filter(|token| token.active_leaf && token.member_matches_active_leaf)
            .count();
        let stale_token_count = tokens.len() - active_token_count;
        let missing_relay_hint_count = tokens.iter().filter(|token| !token.has_relay_hint).count();

        Ok(GroupPushDebugInfo {
            total_token_count: cached_tokens.len(),
            active_token_count,
            stale_token_count,
            missing_relay_hint_count,
            last_token_list_updated_at,
            local_registration: LocalPushRegistrationDebugInfo {
                registered: registration.is_some(),
                shareable,
                notifications_enabled,
                local_leaf_index,
                local_token_cached,
            },
            tokens,
        })
    }

    // ── Crate-internal API ────────────────────────────────────────────

    /// Handles an incoming MIP-05 group message (token request, response, or
    /// removal).
    #[perf_instrument("push_notifications")]
    pub(crate) async fn handle_received_push_group_message(
        &self,
        message: &Message,
        sender_leaf_index: Option<u32>,
    ) -> Result<bool> {
        if !is_push_group_message_kind(message.kind) {
            return Ok(false);
        }

        let group_message = parse_group_message_rumor(&message.event)?;

        match group_message {
            Mip05GroupMessage::TokenRequest(request) => {
                let source = request.source;
                let leaf_index = source
                    .map(|source| source.leaf_index)
                    .or(sender_leaf_index)
                    .ok_or_else(|| {
                        WhitenoiseError::InvalidEvent(
                            "MIP-05 token request missing sender leaf index".to_string(),
                        )
                    })?;
                let member_pubkey = source
                    .map(|source| source.member_pubkey)
                    .unwrap_or(message.event.pubkey);

                let request_event_id = message.event.id;
                self.merge_token_request(&message.mls_group_id, member_pubkey, leaf_index, request)
                    .await?;

                if !self.check_token_request_rate(
                    &message.mls_group_id,
                    leaf_index,
                    TokenRateKind::Request,
                ) {
                    tracing::debug!(
                        target: "whitenoise::push_notifications",
                        group_id = %hex::encode(message.mls_group_id.as_slice()),
                        leaf_index,
                        "Dropping rate-limited MIP-05 token request"
                    );
                    return Ok(true);
                }

                if let Some(request_event_id) = request_event_id
                    && source.is_none()
                {
                    self.session.schedule_pending_token_response(
                        message.mls_group_id.clone(),
                        request_event_id,
                    );
                }
            }
            Mip05GroupMessage::TokenListResponse(response) => {
                let request_event_id = response.request_event_id;
                self.merge_token_list_response(&message.mls_group_id, response)
                    .await?;
                if let Some(request_event_id) = request_event_id {
                    self.clear_pending_response(&message.mls_group_id, &request_event_id);
                }
            }
            Mip05GroupMessage::TokenRemoval(removal) => {
                if !removal.member_pubkeys.is_empty() {
                    for member_pubkey in removal.member_pubkeys {
                        self.session
                            .repos
                            .group_push_tokens
                            .delete_by_member_pubkey(&message.mls_group_id, &member_pubkey)
                            .await?;
                    }
                    return Ok(true);
                }

                let leaf_index = sender_leaf_index.ok_or_else(|| {
                    WhitenoiseError::InvalidEvent(
                        "MIP-05 token removal missing sender leaf index".to_string(),
                    )
                })?;
                if !self.check_token_request_rate(
                    &message.mls_group_id,
                    leaf_index,
                    TokenRateKind::Removal,
                ) {
                    tracing::debug!(
                        target: "whitenoise::push_notifications",
                        group_id = %hex::encode(message.mls_group_id.as_slice()),
                        leaf_index,
                        "Dropping rate-limited MIP-05 token removal"
                    );
                    return Ok(true);
                }

                self.session
                    .repos
                    .group_push_tokens
                    .delete(&message.mls_group_id, leaf_index)
                    .await?;
            }
        }

        Ok(true)
    }

    /// Dispatches a pending MIP-05 token-list response if one is scheduled.
    #[perf_instrument("push_notifications")]
    pub(crate) async fn dispatch_pending_token_response(
        &self,
        group_id: &GroupId,
        request_event_id: EventId,
    ) -> Result<bool> {
        if !self.clear_pending_response(group_id, &request_event_id) {
            return Ok(false);
        }

        self.respond_to_token_request(group_id, request_event_id)
            .await?;
        Ok(true)
    }

    /// Shares the locally registered push token to all joined groups where
    /// notifications are enabled.
    #[perf_instrument("push_notifications")]
    pub(crate) async fn share_local_token_to_joined_groups(&self) -> Result<()> {
        if !self.session.repos.settings.notifications_enabled().await? {
            return Ok(());
        }

        let Some(registration) = self.registration().await? else {
            return Ok(());
        };
        let Some(token_tag) = registration.token_tag()? else {
            return Ok(());
        };

        self.share_token_to_joined_groups(&registration, &token_tag)
            .await
    }

    /// Shares the locally registered push token to a single group.
    #[perf_instrument("push_notifications")]
    pub(crate) async fn share_local_token_to_group(&self, group_id: &GroupId) -> Result<()> {
        let group_state = self.push_gossip_group_state(group_id)?;
        if !self.is_push_gossip_eligible(group_id, group_state).await {
            return Ok(());
        }

        if !self.session.repos.settings.notifications_enabled().await? {
            return Ok(());
        }

        let Some(registration) = self.registration().await? else {
            return Ok(());
        };
        let Some(token_tag) = registration.token_tag()? else {
            return Ok(());
        };

        self.share_token_to_group(group_id, &registration, &token_tag)
            .await
    }

    /// Removes the local push token from all joined active groups.
    #[perf_instrument("push_notifications")]
    pub(crate) async fn remove_local_token_from_joined_groups(&self) -> Result<()> {
        let registration = self.registration().await?;
        self.remove_registration_token_from_joined_groups(registration)
            .await
    }

    #[perf_instrument("push_notifications")]
    async fn remove_registration_token_from_joined_groups(
        &self,
        registration: Option<PushRegistration>,
    ) -> Result<()> {
        let group_pairs = self.push_gossip_group_targets()?;
        let failures: Vec<String> = stream::iter(group_pairs.into_iter())
            .map(|(gid, state)| {
                let registration = registration.clone();
                async move {
                    self.remove_token_from_single_group(&gid, state, registration.as_ref())
                        .await
                }
            })
            .buffer_unordered(MAX_CONCURRENT_GROUP_PUBLISHES)
            .filter_map(|r| async move { r.err() })
            .collect()
            .await;

        if failures.is_empty() {
            Ok(())
        } else {
            Err(WhitenoiseError::Configuration(format!(
                "failed to remove push token from one or more groups: {}",
                failures.join(", ")
            )))
        }
    }

    /// Removes the local push token from a single group.
    #[perf_instrument("push_notifications")]
    pub(crate) async fn remove_local_token_from_group(&self, group_id: &GroupId) -> Result<()> {
        let group_state = self.push_gossip_group_state(group_id)?;

        if group_state != GroupState::Active {
            self.session
                .repos
                .group_push_tokens
                .delete_by_member_pubkey(group_id, &self.session.account_pubkey)
                .await?;
            return Ok(());
        }

        let registration = self.registration().await?;
        if let Some(registration) = registration.as_ref() {
            if let Err(error) = self.publish_token_removal(group_id, registration).await {
                tracing::warn!(
                    target: "whitenoise::push_notifications",
                    account = %self.session.account_pubkey.to_hex(),
                    group = %hex::encode(group_id.as_slice()),
                    error = %error,
                    "Failed to publish token removal to group"
                );
            }
        } else {
            self.session
                .repos
                .group_push_tokens
                .delete_by_member_pubkey(group_id, &self.session.account_pubkey)
                .await?;
            return Ok(());
        }

        self.sync_local_group_push_token_cache(group_id, None).await
    }

    /// Reconciles cached group push tokens with the current active leaf map.
    #[perf_instrument("push_notifications")]
    pub(crate) async fn reconcile_group_tokens_for_active_leaves(
        &self,
        group_id: &GroupId,
    ) -> Result<()> {
        let active_leaf_map = self.active_push_member_index_map(group_id).await?;
        let cached_tokens = self
            .session
            .repos
            .group_push_tokens
            .find_by_group(group_id)
            .await?;

        for token in cached_tokens {
            match active_leaf_map.get(&token.leaf_index) {
                Some(pk) if pk == &token.member_pubkey => continue,
                None if !active_leaf_map
                    .values()
                    .any(|pk| pk == &token.member_pubkey) =>
                {
                    self.session
                        .repos
                        .group_push_tokens
                        .delete_by_member_pubkey(group_id, &token.member_pubkey)
                        .await?;
                }
                _ => {
                    self.session
                        .repos
                        .group_push_tokens
                        .delete(group_id, token.leaf_index)
                        .await?;
                }
            }
        }

        Ok(())
    }

    // ── Private helpers ───────────────────────────────────────────────

    #[perf_instrument("push_notifications")]
    async fn share_token_to_joined_groups(
        &self,
        registration: &PushRegistration,
        token_tag: &TokenTag,
    ) -> Result<()> {
        let group_pairs = self.push_gossip_group_targets()?;
        let failures: Vec<String> = stream::iter(group_pairs.into_iter())
            .map(|(gid, state)| async move {
                if !self.is_push_gossip_eligible(&gid, state).await {
                    return Ok(());
                }
                self.share_token_to_group(&gid, registration, token_tag)
                    .await
                    .map_err(|e| format!("{}: {e}", hex::encode(gid.as_slice())))
            })
            .buffer_unordered(MAX_CONCURRENT_GROUP_PUBLISHES)
            .filter_map(|r| async move { r.err() })
            .collect()
            .await;

        if failures.is_empty() {
            Ok(())
        } else {
            Err(WhitenoiseError::Configuration(format!(
                "failed to share push token to one or more groups: {}",
                failures.join(", ")
            )))
        }
    }

    #[perf_instrument("push_notifications")]
    async fn share_token_to_group(
        &self,
        group_id: &GroupId,
        registration: &PushRegistration,
        token_tag: &TokenTag,
    ) -> Result<()> {
        self.publish_darkmatter_token_update(group_id, registration, token_tag)
            .await?;

        self.sync_local_group_push_token_cache(group_id, Some(token_tag))
            .await
    }

    #[perf_instrument("push_notifications")]
    async fn remove_token_from_single_group(
        &self,
        group_id: &GroupId,
        group_state: GroupState,
        registration: Option<&PushRegistration>,
    ) -> std::result::Result<(), String> {
        if group_state != GroupState::Active {
            // Clean up stale cache rows for inactive groups (no relay publish
            // possible without an active MLS group).
            self.session
                .repos
                .group_push_tokens
                .delete_by_member_pubkey(group_id, &self.session.account_pubkey)
                .await
                .map_err(|e| format!("{}: {e}", hex::encode(group_id.as_slice())))?;
            return Ok(());
        }

        let Some(registration) = registration else {
            self.session
                .repos
                .group_push_tokens
                .delete_by_member_pubkey(group_id, &self.session.account_pubkey)
                .await
                .map_err(|e| format!("{}: {e}", hex::encode(group_id.as_slice())))?;
            return Ok(());
        };

        let group_id_hex = hex::encode(group_id.as_slice());
        let publish_err = self
            .publish_token_removal(group_id, registration)
            .await
            .err()
            .map(|e| format!("{group_id_hex}: {e}"));

        let cache_err = self
            .sync_local_group_push_token_cache(group_id, None)
            .await
            .err()
            .map(|e| format!("{group_id_hex}: {e}"));

        match (publish_err, cache_err) {
            (None, None) => Ok(()),
            (Some(e), None) | (None, Some(e)) => Err(e),
            (Some(e1), Some(e2)) => Err(format!("{e1}, {e2}")),
        }
    }

    /// Returns `true` when push-gossip is allowed for the given group.
    async fn is_push_gossip_eligible(&self, group_id: &GroupId, group_state: GroupState) -> bool {
        if group_state != GroupState::Active {
            return false;
        }
        match AccountGroup::find_by_account_and_group(
            &self.session.account_pubkey,
            group_id,
            &self.session.account_db.inner.pool,
        )
        .await
        {
            Ok(Some(ag)) => ag.is_accepted() && !ag.is_removed(),
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(
                    target: "whitenoise::push_notifications",
                    account = %self.session.account_pubkey.to_hex(),
                    group = %hex::encode(group_id.as_slice()),
                    error = %error,
                    "Failed to check push gossip eligibility; treating as ineligible"
                );
                false
            }
        }
    }

    /// Returns `true` if the request is allowed (not rate-limited).
    fn check_token_request_rate(
        &self,
        group_id: &GroupId,
        leaf_index: u32,
        kind: TokenRateKind,
    ) -> bool {
        let key = (
            self.session.account_pubkey,
            group_id.clone(),
            leaf_index,
            kind,
        );
        let now = Instant::now();
        let mut allowed = false;

        self.session
            .shared
            .token_request_timestamps
            .entry(key)
            .and_modify(|last| {
                if now.duration_since(*last) >= TOKEN_REQUEST_COOLDOWN {
                    *last = now;
                    allowed = true;
                }
            })
            .or_insert_with(|| {
                allowed = true;
                now
            });

        allowed
    }

    #[perf_instrument("push_notifications")]
    async fn merge_token_request(
        &self,
        mls_group_id: &GroupId,
        member_pubkey: PublicKey,
        leaf_index: u32,
        request: TokenRequest,
    ) -> Result<()> {
        let Some(token) = request.tokens.into_iter().next() else {
            return Err(WhitenoiseError::InvalidEvent(
                "MIP-05 token request must include at least one token".to_string(),
            ));
        };
        let encrypted_token = token.encrypted_token.to_base64();

        self.session
            .repos
            .group_push_tokens
            .upsert_with_metadata(GroupPushTokenUpsert {
                mls_group_id,
                member_pubkey: &member_pubkey,
                leaf_index,
                server_pubkey: &token.server_pubkey,
                relay_hint: Some(&token.relay_hint),
                encrypted_token: &encrypted_token,
                platform: token.platform.map(PushPlatform::from_notification_platform),
                token_fingerprint: token.token_fingerprint.as_deref(),
            })
            .await?;

        Ok(())
    }

    #[perf_instrument("push_notifications")]
    async fn merge_token_list_response(
        &self,
        mls_group_id: &GroupId,
        response: TokenListResponse,
    ) -> Result<()> {
        let active_leaf_map = self.active_push_member_index_map(mls_group_id).await?;

        self.session
            .repos
            .group_push_tokens
            .upsert_active_token_list_response(mls_group_id, &active_leaf_map, response.tokens)
            .await?;

        Ok(())
    }

    #[perf_instrument("push_notifications")]
    async fn active_push_member_index_map(
        &self,
        group_id: &GroupId,
    ) -> Result<BTreeMap<u32, PublicKey>> {
        self.response_context()
            .active_push_member_index_map(group_id)
            .await
    }

    fn push_gossip_group_state(&self, group_id: &GroupId) -> Result<GroupState> {
        let marmot_group_id = MarmotGroupId::new(group_id.as_slice().to_vec());
        if self
            .session
            .marmot_storage
            .find_group_projection(&marmot_group_id)?
            .is_some()
        {
            return Ok(GroupState::Active);
        }

        Ok(GroupState::Inactive)
    }

    fn push_gossip_group_targets(&self) -> Result<Vec<(GroupId, GroupState)>> {
        let mut targets = Vec::new();

        for projection in self.session.marmot_storage.list_group_projections()? {
            let group_id = GroupId::from_slice(projection.group_id.as_slice());
            targets.push((group_id, GroupState::Active));
        }

        Ok(targets)
    }

    async fn publish_darkmatter_token_update(
        &self,
        group_id: &GroupId,
        registration: &PushRegistration,
        token_tag: &TokenTag,
    ) -> Result<()> {
        let active_leaf_map = self.active_push_member_index_map(group_id).await?;
        let leaf_index = self
            .local_push_member_index(&active_leaf_map)
            .ok_or(WhitenoiseError::GroupNotFound)?;
        let plaintext = registration.push_token_plaintext()?;
        let fingerprint = push_token_fingerprint(plaintext.platform(), plaintext.device_token());
        let event = build_token_update_app_event(
            self.session.account_pubkey,
            nostr_sdk::Timestamp::now(),
            self.session.account_pubkey,
            leaf_index,
            plaintext.platform(),
            &fingerprint,
            token_tag.clone(),
        )?;

        self.publish_darkmatter_push_event(group_id, event).await
    }

    async fn publish_token_removal(
        &self,
        group_id: &GroupId,
        registration: &PushRegistration,
    ) -> Result<()> {
        self.publish_darkmatter_token_removal(group_id, registration)
            .await
    }

    async fn publish_darkmatter_token_removal(
        &self,
        group_id: &GroupId,
        registration: &PushRegistration,
    ) -> Result<()> {
        let event =
            build_darkmatter_token_removal_event(registration, nostr_sdk::Timestamp::now())?;
        self.publish_darkmatter_push_event(group_id, event).await
    }

    async fn publish_darkmatter_push_event(
        &self,
        group_id: &GroupId,
        event: nostr_sdk::UnsignedEvent,
    ) -> Result<()> {
        self.response_context()
            .publish_darkmatter_push_event(group_id, event)
            .await
    }

    fn local_push_member_index(&self, member_index: &BTreeMap<u32, PublicKey>) -> Option<u32> {
        member_index
            .iter()
            .find_map(|(index, pubkey)| (*pubkey == self.session.account_pubkey).then_some(*index))
    }

    #[perf_instrument("push_notifications")]
    async fn respond_to_token_request(
        &self,
        group_id: &GroupId,
        request_event_id: EventId,
    ) -> Result<()> {
        self.response_context()
            .respond_to_token_request(group_id, request_event_id)
            .await
    }

    fn response_context(&self) -> PushResponseContext {
        PushResponseContext::from_session(self.session)
    }

    #[perf_instrument("push_notifications")]
    async fn sync_local_group_push_token_cache(
        &self,
        group_id: &GroupId,
        token_tag: Option<&TokenTag>,
    ) -> Result<()> {
        let active_leaf_map = self.active_push_member_index_map(group_id).await?;
        let leaf_index = self
            .local_push_member_index(&active_leaf_map)
            .ok_or(WhitenoiseError::GroupNotFound)?;

        match token_tag {
            Some(token_tag) => {
                let encrypted_token = token_tag.encrypted_token.to_base64();
                self.session
                    .repos
                    .group_push_tokens
                    .upsert_with_metadata(GroupPushTokenUpsert {
                        mls_group_id: group_id,
                        member_pubkey: &self.session.account_pubkey,
                        leaf_index,
                        server_pubkey: &token_tag.server_pubkey,
                        relay_hint: Some(&token_tag.relay_hint),
                        encrypted_token: &encrypted_token,
                        platform: token_tag
                            .platform
                            .map(PushPlatform::from_notification_platform),
                        token_fingerprint: token_tag.token_fingerprint.as_deref(),
                    })
                    .await?;
            }
            None => {
                self.session
                    .repos
                    .group_push_tokens
                    .delete(group_id, leaf_index)
                    .await?;
            }
        }

        Ok(())
    }

    #[perf_instrument("push_notifications")]
    async fn local_push_token_tag(&self) -> Result<Option<TokenTag>> {
        let Some(registration) = self.registration().await? else {
            return Ok(None);
        };

        registration.token_tag()
    }

    fn clear_pending_response(&self, group_id: &GroupId, request_event_id: &EventId) -> bool {
        self.session
            .pending_push_token_responses
            .remove(&(group_id.clone(), *request_event_id))
            .is_some()
    }
}

fn build_darkmatter_token_removal_event(
    registration: &PushRegistration,
    created_at: nostr_sdk::Timestamp,
) -> Result<nostr_sdk::UnsignedEvent> {
    let plaintext = registration.push_token_plaintext()?;
    let fingerprint = push_token_fingerprint(plaintext.platform(), plaintext.device_token());
    Ok(build_token_removal_app_event(
        registration.account_pubkey,
        created_at,
        registration.account_pubkey,
        plaintext.platform(),
        &fingerprint,
        registration.server_pubkey,
    )?)
}

fn is_missing_marmot_group_error(error: &WhitenoiseError) -> bool {
    matches!(
        error,
        WhitenoiseError::MarmotEngine(cgka_traits::EngineError::UnknownGroup(_))
            | WhitenoiseError::MarmotEngine(cgka_traits::EngineError::Storage(
                StorageError::NotFound,
            ))
            | WhitenoiseError::MarmotStorage(StorageError::NotFound)
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Weak;
    use std::time::{Duration, Instant};

    use cgka_traits::MessageId as MarmotMessageId;
    use cgka_traits::app_components::{
        AppComponentData, NOSTR_ROUTING_COMPONENT_ID, NostrRoutingV1, encode_nostr_routing_v1,
    };
    use cgka_traits::engine::CreateGroupRequest;
    use nostr_sdk::{EventId, Keys, Kind, RelayUrl, Timestamp};

    use super::*;
    use crate::marmot::push::{
        EncryptedToken, Mip05GroupMessage, NotificationPlatform, TOKEN_LIST_RESPONSE_KIND,
        TOKEN_REMOVAL_KIND, TOKEN_REQUEST_KIND, TokenListResponse, build_token_removal_app_event,
        build_token_request_rumor, build_token_update_app_event, parse_group_message_rumor,
        push_token_fingerprint,
    };
    use crate::marmot::session::{MarmotSession, PublishWork};
    use crate::marmot::storage::WhitenoiseMarmotStorage;
    use crate::marmot::{MarmotCreatedGroupProjection, Message, MessageState};
    use crate::whitenoise::Whitenoise;
    use crate::whitenoise::session::test_helpers::{
        test_db, test_session, test_session_with_marmot_keys, test_shared,
    };
    use crate::whitenoise::test_utils::{
        ObsoleteMlsArtifacts, assert_obsolete_mls_artifacts_absent,
        count_published_events_for_account, create_mock_whitenoise, insert_test_account,
        setup_unprojected_accepted_group,
    };

    fn obsolete_mls_artifacts_for_session(session: &AccountSession) -> ObsoleteMlsArtifacts {
        ObsoleteMlsArtifacts {
            storage_path: crate::whitenoise::test_utils::obsolete_mls_storage_path(
                &session.account_pubkey,
                &session.shared.config.data_dir,
            ),
        }
    }

    fn assert_obsolete_mls_artifacts_absent_for_session(session: &AccountSession) {
        let artifacts = obsolete_mls_artifacts_for_session(session);
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn push_group_message_handler_accepts_marmot_message_projection() {
        let account_keys = Keys::generate();
        let sender = Keys::generate();
        let session = test_session(account_keys.public_key()).await;
        let group_id = GroupId::from_slice(&[9; 32]);
        let message = token_request_message(&sender, group_id.clone());

        let handled = session
            .push()
            .handle_received_push_group_message(&message, Some(5))
            .await
            .unwrap();

        assert!(handled);
        let cached_tokens = session
            .repos
            .group_push_tokens
            .find_by_group(&group_id)
            .await
            .unwrap();
        assert_eq!(cached_tokens.len(), 1);
        assert_eq!(cached_tokens[0].member_pubkey, sender.public_key());
        assert_eq!(cached_tokens[0].leaf_index, 5);
    }

    #[tokio::test]
    async fn push_group_message_handler_accepts_darkmatter_json_token_update() {
        let account_keys = Keys::generate();
        let sender = Keys::generate();
        let session = test_session(account_keys.public_key()).await;
        let group_id = GroupId::from_slice(&[10; 32]);
        let message = token_update_message(&sender, group_id.clone(), 8);
        let expected_fingerprint =
            push_token_fingerprint(NotificationPlatform::Fcm, b"firebase-token");

        let handled = session
            .push()
            .handle_received_push_group_message(&message, None)
            .await
            .unwrap();

        assert!(handled);
        let cached_tokens = session
            .repos
            .group_push_tokens
            .find_by_group(&group_id)
            .await
            .unwrap();
        assert_eq!(cached_tokens.len(), 1);
        assert_eq!(cached_tokens[0].member_pubkey, sender.public_key());
        assert_eq!(cached_tokens[0].leaf_index, 8);
        assert_eq!(cached_tokens[0].platform, Some(PushPlatform::Fcm));
        assert_eq!(
            cached_tokens[0].token_fingerprint.as_deref(),
            Some(expected_fingerprint.as_str())
        );
    }

    #[tokio::test]
    async fn push_group_message_handler_accepts_darkmatter_json_token_removal() {
        let account_keys = Keys::generate();
        let sender = Keys::generate();
        let session = test_session(account_keys.public_key()).await;
        let group_id = GroupId::from_slice(&[11; 32]);
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();

        session
            .repos
            .group_push_tokens
            .upsert(
                &group_id,
                &sender.public_key(),
                9,
                &server_pubkey,
                Some(&relay_hint),
                "ciphertext",
            )
            .await
            .unwrap();

        let message = token_removal_message(&sender, group_id.clone(), server_pubkey);
        let handled = session
            .push()
            .handle_received_push_group_message(&message, None)
            .await
            .unwrap();

        assert!(handled);
        let cached_tokens = session
            .repos
            .group_push_tokens
            .find_by_group(&group_id)
            .await
            .unwrap();
        assert!(cached_tokens.is_empty());
    }

    #[tokio::test]
    async fn rate_limiter_allows_first_request() {
        let account_keys = Keys::generate();
        let session = test_session(account_keys.public_key()).await;
        let group_id = GroupId::from_slice(&[42; 32]);

        assert!(
            session
                .push()
                .check_token_request_rate(&group_id, 0, TokenRateKind::Request)
        );
    }

    #[tokio::test]
    async fn rate_limiter_blocks_rapid_duplicate() {
        let account_keys = Keys::generate();
        let session = test_session(account_keys.public_key()).await;
        let group_id = GroupId::from_slice(&[42; 32]);

        assert!(
            session
                .push()
                .check_token_request_rate(&group_id, 0, TokenRateKind::Request)
        );
        assert!(
            !session
                .push()
                .check_token_request_rate(&group_id, 0, TokenRateKind::Request)
        );
    }

    #[tokio::test]
    async fn rate_limiter_allows_different_senders() {
        let account_keys = Keys::generate();
        let session = test_session(account_keys.public_key()).await;
        let group_id = GroupId::from_slice(&[42; 32]);

        assert!(
            session
                .push()
                .check_token_request_rate(&group_id, 0, TokenRateKind::Request)
        );
        assert!(
            session
                .push()
                .check_token_request_rate(&group_id, 1, TokenRateKind::Request)
        );
    }

    #[tokio::test]
    async fn rate_limiter_allows_different_groups() {
        let account_keys = Keys::generate();
        let session = test_session(account_keys.public_key()).await;
        let group_a = GroupId::from_slice(&[42; 32]);
        let group_b = GroupId::from_slice(&[43; 32]);

        assert!(
            session
                .push()
                .check_token_request_rate(&group_a, 0, TokenRateKind::Request)
        );
        assert!(
            session
                .push()
                .check_token_request_rate(&group_b, 0, TokenRateKind::Request)
        );
    }

    #[tokio::test]
    async fn rate_limiter_allows_different_accounts() {
        let first_keys = Keys::generate();
        let second_keys = Keys::generate();
        let first_session = test_session(first_keys.public_key()).await;
        let second_session = AccountSession::new(
            second_keys.public_key(),
            Arc::clone(&first_session.shared),
            Weak::new(),
            Some(Arc::new(second_keys.clone())),
            None,
        )
        .await
        .unwrap();
        let group_id = GroupId::from_slice(&[42; 32]);

        assert!(first_session.push().check_token_request_rate(
            &group_id,
            0,
            TokenRateKind::Request
        ));
        assert!(second_session.push().check_token_request_rate(
            &group_id,
            0,
            TokenRateKind::Request
        ));
    }

    #[tokio::test]
    async fn rate_limiter_request_does_not_suppress_removal() {
        let account_keys = Keys::generate();
        let session = test_session(account_keys.public_key()).await;
        let group_id = GroupId::from_slice(&[42; 32]);

        assert!(
            session
                .push()
                .check_token_request_rate(&group_id, 0, TokenRateKind::Request)
        );
        assert!(
            session
                .push()
                .check_token_request_rate(&group_id, 0, TokenRateKind::Removal)
        );
    }

    #[tokio::test]
    async fn rate_limiter_wires_into_session_push_handler() {
        let account_keys = Keys::generate();
        let sender = Keys::generate();
        let session = test_session(account_keys.public_key()).await;
        let group_id = GroupId::from_slice(&[77; 32]);
        let leaf_index = 3;
        let first_message = token_request_message(&sender, group_id.clone());
        let second_message = token_request_message(&sender, group_id.clone());
        let first_event_id = first_message
            .event
            .id
            .expect("first request should have id");
        let second_event_id = second_message
            .event
            .id
            .expect("second request should have id");

        let first_handled = session
            .push()
            .handle_received_push_group_message(&first_message, Some(leaf_index))
            .await
            .unwrap();
        let second_handled = session
            .push()
            .handle_received_push_group_message(&second_message, Some(leaf_index))
            .await
            .unwrap();

        assert!(first_handled);
        assert!(second_handled);
        assert!(
            session
                .pending_push_token_responses
                .contains_key(&(group_id.clone(), first_event_id)),
            "first request must schedule a pending token-list response"
        );
        assert!(
            !session
                .pending_push_token_responses
                .contains_key(&(group_id, second_event_id)),
            "rate-limited request must not schedule another pending token-list response"
        );
    }

    #[tokio::test]
    async fn rate_limiter_allows_after_cooldown() {
        let account_keys = Keys::generate();
        let account_pubkey = account_keys.public_key();
        let session = test_session(account_pubkey).await;
        let group_id = GroupId::from_slice(&[42; 32]);

        assert!(
            session
                .push()
                .check_token_request_rate(&group_id, 0, TokenRateKind::Request)
        );

        let key = (
            account_pubkey,
            group_id.clone(),
            0u32,
            TokenRateKind::Request,
        );
        let backdated = Instant::now()
            .checked_sub(TOKEN_REQUEST_COOLDOWN + Duration::from_millis(1))
            .expect("system uptime must exceed cooldown window");
        session
            .shared
            .token_request_timestamps
            .insert(key, backdated);

        assert!(
            session
                .push()
                .check_token_request_rate(&group_id, 0, TokenRateKind::Request)
        );
    }

    #[test]
    fn darkmatter_token_removal_event_uses_registration_being_removed() {
        let account_keys = Keys::generate();
        let server_pubkey = Keys::generate().public_key();
        let registration = PushRegistration {
            account_pubkey: account_keys.public_key(),
            platform: PushPlatform::Fcm,
            raw_token: "firebase-token".to_string(),
            server_pubkey,
            relay_hint: RelayUrl::parse("wss://push.example.com").ok(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            last_shared_at: None,
        };
        let expected_fingerprint =
            push_token_fingerprint(NotificationPlatform::Fcm, b"firebase-token");

        let event =
            build_darkmatter_token_removal_event(&registration, Timestamp::from(123)).unwrap();

        assert_eq!(event.pubkey, account_keys.public_key());
        assert_eq!(event.kind, Kind::from(TOKEN_REMOVAL_KIND));
        assert!(
            event
                .tags
                .iter()
                .any(|tag| tag.as_slice() == ["v", "mip05-v1"])
        );
        let payload: serde_json::Value = serde_json::from_str(&event.content).unwrap();
        assert_eq!(payload["v"], "mip05-v1");
        assert_eq!(
            payload["removals"][0]["member_id_hex"],
            account_keys.public_key().to_hex()
        );
        assert_eq!(payload["removals"][0]["platform"], "fcm");
        assert_eq!(
            payload["removals"][0]["token_fingerprint"],
            expected_fingerprint
        );
        assert_eq!(
            payload["removals"][0]["server_pubkey_hex"],
            server_pubkey.to_hex()
        );
    }

    #[tokio::test]
    async fn push_debug_info_uses_darkmatter_members_for_projected_group() {
        let account_keys = Keys::generate();
        let session = test_session_with_marmot_keys(account_keys.clone()).await;
        let (group_id, _bob_pubkey) = create_confirmed_marmot_group(&session).await;
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();

        session
            .repos
            .group_push_tokens
            .upsert(
                &group_id,
                &account_keys.public_key(),
                0,
                &server_pubkey,
                Some(&relay_hint),
                "ciphertext",
            )
            .await
            .unwrap();

        let debug = session
            .push()
            .get_group_debug_info(&group_id)
            .await
            .unwrap();

        assert_eq!(debug.active_token_count, 1);
        assert_eq!(debug.stale_token_count, 0);
        assert_eq!(debug.local_registration.local_leaf_index, Some(0));
        assert!(debug.local_registration.local_token_cached);
        assert_obsolete_mls_artifacts_absent_for_session(&session);
    }

    #[tokio::test]
    async fn darkmatter_token_list_response_event_uses_cached_metadata_for_projected_group() {
        let account_keys = Keys::generate();
        let session = test_session_with_marmot_keys(account_keys.clone()).await;
        let (group_id, bob_pubkey) = create_confirmed_marmot_group(&session).await;
        let active_leaf_map = session
            .push()
            .active_push_member_index_map(&group_id)
            .await
            .unwrap();
        let bob_leaf = active_leaf_map
            .iter()
            .find_map(|(leaf, pubkey)| (*pubkey == bob_pubkey).then_some(*leaf))
            .unwrap();
        let encrypted_token = EncryptedToken::from([3u8; 1084]);
        let encrypted_token_b64 = encrypted_token.to_base64();
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let fingerprint = push_token_fingerprint(NotificationPlatform::Fcm, b"firebase-token");

        session
            .repos
            .group_push_tokens
            .upsert_with_metadata(GroupPushTokenUpsert {
                mls_group_id: &group_id,
                member_pubkey: &bob_pubkey,
                leaf_index: bob_leaf,
                server_pubkey: &server_pubkey,
                relay_hint: Some(&relay_hint),
                encrypted_token: &encrypted_token_b64,
                platform: Some(PushPlatform::Fcm),
                token_fingerprint: Some(&fingerprint),
            })
            .await
            .unwrap();

        let event = PushResponseContext::from_session(&session)
            .build_darkmatter_token_list_response_event(&group_id)
            .await
            .unwrap()
            .unwrap();
        let parsed = parse_group_message_rumor(&event).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&event.content).unwrap();

        assert_eq!(event.pubkey, account_keys.public_key());
        assert_eq!(event.kind, Kind::from(TOKEN_LIST_RESPONSE_KIND));
        assert_eq!(payload["v"], "mip05-v1");
        assert_eq!(payload["tokens"][0]["member_id_hex"], bob_pubkey.to_hex());
        assert_eq!(payload["tokens"][0]["leaf_index"], bob_leaf);
        assert_eq!(payload["tokens"][0]["platform"], "fcm");
        assert_eq!(payload["tokens"][0]["token_fingerprint"], fingerprint);
        assert_eq!(
            parsed,
            Mip05GroupMessage::TokenListResponse(TokenListResponse {
                request_event_id: None,
                tokens: vec![crate::marmot::push::LeafTokenTag {
                    leaf_index: bob_leaf,
                    token_tag: TokenTag {
                        encrypted_token,
                        platform: Some(NotificationPlatform::Fcm),
                        token_fingerprint: Some(fingerprint),
                        server_pubkey,
                        relay_hint,
                    },
                }],
            })
        );
    }

    #[tokio::test]
    async fn after_delivery_notification_request_uses_darkmatter_members_without_obsolete_mls_storage()
     {
        let account_keys = Keys::generate();
        let session = test_session_with_marmot_keys(account_keys.clone()).await;
        let (group_id, bob_pubkey) = create_confirmed_marmot_group(&session).await;
        let active_leaf_map = session
            .push()
            .active_push_member_index_map(&group_id)
            .await
            .unwrap();
        let bob_leaf = active_leaf_map
            .iter()
            .find_map(|(leaf, pubkey)| (*pubkey == bob_pubkey).then_some(*leaf))
            .unwrap();
        let encrypted_token = EncryptedToken::from([5u8; 1084]);
        let server_pubkey = Keys::generate().public_key();
        let fingerprint = push_token_fingerprint(NotificationPlatform::Fcm, b"after-delivery");

        session
            .repos
            .group_push_tokens
            .upsert_with_metadata(GroupPushTokenUpsert {
                mls_group_id: &group_id,
                member_pubkey: &bob_pubkey,
                leaf_index: bob_leaf,
                server_pubkey: &server_pubkey,
                relay_hint: None,
                encrypted_token: &encrypted_token.to_base64(),
                platform: Some(PushPlatform::Fcm),
                token_fingerprint: Some(&fingerprint),
            })
            .await
            .unwrap();

        PushResponseContext::from_session(&session)
            .publish_notification_requests_after_delivery(&group_id)
            .await
            .unwrap();

        assert_obsolete_mls_artifacts_absent_for_session(&session);
    }

    #[tokio::test]
    async fn projected_darkmatter_push_member_index_without_live_session_does_not_fall_back_to_obsolete_mls_storage()
     {
        let account_keys = Keys::generate();
        let account_pubkey = account_keys.public_key();
        let (session, group_id) =
            test_session_with_projected_darkmatter_group_without_live_session(account_keys).await;

        let result = PushResponseContext::from_session(&session)
            .active_push_member_index_map(&group_id)
            .await;

        assert!(matches!(
            result,
            Err(WhitenoiseError::MarmotSessionUnavailable(pubkey)) if pubkey == account_pubkey
        ));
        assert_obsolete_mls_artifacts_absent_for_session(&session);
    }

    #[tokio::test]
    async fn projected_darkmatter_push_publish_without_live_session_does_not_fall_back_to_obsolete_mls_storage()
     {
        let account_keys = Keys::generate();
        let account_pubkey = account_keys.public_key();
        let (session, group_id) =
            test_session_with_projected_darkmatter_group_without_live_session(account_keys).await;
        let token_fingerprint =
            push_token_fingerprint(NotificationPlatform::Fcm, b"firebase-token");
        let event = build_token_removal_app_event(
            account_pubkey,
            Timestamp::from(123),
            account_pubkey,
            NotificationPlatform::Fcm,
            &token_fingerprint,
            Keys::generate().public_key(),
        )
        .unwrap();

        let result = PushResponseContext::from_session(&session)
            .publish_darkmatter_push_event(&group_id, event)
            .await;

        assert!(matches!(
            result,
            Err(WhitenoiseError::MarmotSessionUnavailable(pubkey)) if pubkey == account_pubkey
        ));
        assert_obsolete_mls_artifacts_absent_for_session(&session);
    }

    #[tokio::test]
    async fn push_gossip_targets_skip_unprojected_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let unprojected_group_id =
            setup_unprojected_accepted_group(&whitenoise, &[&creator, &member]).await;
        let session = whitenoise.require_session(&creator.pubkey).unwrap();

        let targets = session.push().push_gossip_group_targets().unwrap();

        assert!(
            targets
                .iter()
                .all(|(group_id, _state)| group_id != &unprojected_group_id),
            "unprojected groups must not be used as push gossip fanout targets"
        );
    }

    #[tokio::test]
    async fn push_gossip_state_treats_unprojected_group_as_inactive() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let unprojected_group_id =
            setup_unprojected_accepted_group(&whitenoise, &[&creator, &member]).await;
        let session = whitenoise.require_session(&creator.pubkey).unwrap();

        let state = session
            .push()
            .push_gossip_group_state(&unprojected_group_id)
            .unwrap();

        assert_eq!(state, GroupState::Inactive);
    }

    #[tokio::test]
    async fn push_debug_info_treats_unprojected_group_tokens_as_stale() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let unprojected_group_id = GroupId::from_slice(&[0xD1; 32]);
        let creator_leaf_index = 0;
        let session = whitenoise.require_session(&creator.pubkey).unwrap();
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();

        session
            .repos
            .group_push_tokens
            .upsert(
                &unprojected_group_id,
                &creator.pubkey,
                creator_leaf_index,
                &server_pubkey,
                Some(&relay_hint),
                "ciphertext",
            )
            .await
            .unwrap();

        let debug = session
            .push()
            .get_group_debug_info(&unprojected_group_id)
            .await
            .unwrap();

        assert_eq!(debug.active_token_count, 0);
        assert_eq!(debug.stale_token_count, 1);
        assert_eq!(debug.local_registration.local_leaf_index, None);
        assert!(!debug.local_registration.local_token_cached);
    }

    #[tokio::test]
    async fn pending_token_response_skips_unprojected_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let unprojected_group_id = GroupId::from_slice(&[0xD2; 32]);
        let creator_leaf_index = 0;
        let session = whitenoise.require_session(&creator.pubkey).unwrap();
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();
        let encrypted_token = EncryptedToken::from([7u8; 1084]);

        session
            .repos
            .group_push_tokens
            .upsert(
                &unprojected_group_id,
                &creator.pubkey,
                creator_leaf_index,
                &server_pubkey,
                Some(&relay_hint),
                &encrypted_token.to_base64(),
            )
            .await
            .unwrap();

        let request_event_id = EventId::all_zeros();
        session
            .pending_push_token_responses
            .insert((unprojected_group_id.clone(), request_event_id), ());
        let before_count = count_published_events_for_account(&whitenoise, &creator).await;

        let dispatched = session
            .push()
            .dispatch_pending_token_response(&unprojected_group_id, request_event_id)
            .await
            .unwrap();
        let after_count = count_published_events_for_account(&whitenoise, &creator).await;

        assert!(dispatched);
        assert_eq!(
            after_count, before_count,
            "unprojected groups must not publish delayed push token-list responses"
        );
    }

    async fn test_session_with_projected_darkmatter_group_without_live_session(
        account_keys: Keys,
    ) -> (AccountSession, GroupId) {
        Whitenoise::initialize_mock_keyring_store();

        let account_pubkey = account_keys.public_key();
        let db = test_db().await;
        insert_test_account(&db, &account_pubkey).await;
        let shared = test_shared(db).await;
        let session = AccountSession::new(
            account_pubkey,
            shared,
            Weak::new(),
            Some(Arc::new(account_keys)),
            None,
        )
        .await
        .unwrap();
        let marmot_group_id = MarmotGroupId::new(vec![0xE1; 32]);
        let group_id = GroupId::from(&marmot_group_id);
        session
            .marmot_storage
            .put_group_projection(&MarmotCreatedGroupProjection {
                group_id: marmot_group_id,
                name: "projected push".to_string(),
                description: "projected push group without live Marmot session".to_string(),
                epoch: 1,
                routing: NostrRoutingV1::new(
                    [0xE2; 32],
                    vec!["wss://projected-push.example".to_string()],
                )
                .unwrap(),
                admin_pubkeys: BTreeSet::from([account_pubkey]),
                member_pubkeys: BTreeSet::from([account_pubkey]),
                self_update_completed_at_secs: 1,
                disappearing_message_secs: None,
            })
            .unwrap();

        (session, group_id)
    }

    async fn create_confirmed_marmot_group(session: &AccountSession) -> (GroupId, PublicKey) {
        let bob_keys = Keys::generate();
        let bob_pubkey = bob_keys.public_key();
        let mut bob = MarmotSession::open_local(
            bob_pubkey,
            WhitenoiseMarmotStorage::in_memory().unwrap(),
            bob_keys,
        )
        .unwrap();
        let bob_key_package = cgka_traits::engine::KeyPackage::with_source_event_id(
            bob.fresh_key_package().await.unwrap().bytes().to_vec(),
            MarmotMessageId::new(vec![0xB0; 32]),
        );

        let marmot_session = session.marmot.as_ref().unwrap();
        let created = {
            let mut marmot = marmot_session.lock().await;
            let created = marmot
                .create_group(CreateGroupRequest {
                    name: "push debug".to_string(),
                    description: String::new(),
                    members: vec![bob_key_package],
                    required_features: Vec::new(),
                    app_components: vec![nostr_routing_component()],
                    initial_admins: Vec::new(),
                })
                .await
                .unwrap();
            let PublishWork::GroupCreated { pending, .. } = &created.effects.publish[0] else {
                panic!("create_group must produce GroupCreated publish work");
            };
            marmot.confirm_published(*pending).await.unwrap();
            created
        };

        (GroupId::from_slice(created.group_id.as_slice()), bob_pubkey)
    }

    fn nostr_routing_component() -> AppComponentData {
        AppComponentData {
            component_id: NOSTR_ROUTING_COMPONENT_ID,
            data: encode_nostr_routing_v1(
                &NostrRoutingV1::new(
                    [0xDD; 32],
                    vec!["wss://push-debug.group.example".to_string()],
                )
                .unwrap(),
            )
            .unwrap(),
        }
    }

    fn token_request_message(sender: &Keys, group_id: GroupId) -> Message {
        let token_tag = TokenTag {
            encrypted_token: EncryptedToken::from_slice(&[0u8; 1084]).unwrap(),
            platform: None,
            token_fingerprint: None,
            server_pubkey: Keys::generate().public_key(),
            relay_hint: RelayUrl::parse("wss://push.example.com").unwrap(),
        };
        let rumor =
            build_token_request_rumor(sender.public_key(), Timestamp::from(123), vec![token_tag])
                .unwrap();

        Message {
            id: rumor.id.unwrap_or(EventId::all_zeros()),
            pubkey: sender.public_key(),
            kind: Kind::from(TOKEN_REQUEST_KIND),
            mls_group_id: group_id,
            created_at: rumor.created_at,
            processed_at: Timestamp::from(124),
            content: rumor.content.clone(),
            tags: rumor.tags.clone(),
            event: rumor,
            wrapper_event_id: EventId::all_zeros(),
            epoch: None,
            state: MessageState::Processed,
        }
    }

    fn token_update_message(sender: &Keys, group_id: GroupId, leaf_index: u32) -> Message {
        let token_tag = TokenTag {
            encrypted_token: EncryptedToken::from_slice(&[1u8; 1084]).unwrap(),
            platform: Some(NotificationPlatform::Fcm),
            token_fingerprint: Some(push_token_fingerprint(
                NotificationPlatform::Fcm,
                b"firebase-token",
            )),
            server_pubkey: Keys::generate().public_key(),
            relay_hint: RelayUrl::parse("wss://push.example.com").unwrap(),
        };
        let fingerprint = push_token_fingerprint(NotificationPlatform::Fcm, b"firebase-token");
        let event = build_token_update_app_event(
            sender.public_key(),
            Timestamp::from(123),
            sender.public_key(),
            leaf_index,
            NotificationPlatform::Fcm,
            &fingerprint,
            token_tag,
        )
        .unwrap();

        Message {
            id: event.id.unwrap_or(EventId::all_zeros()),
            pubkey: sender.public_key(),
            kind: Kind::from(TOKEN_REQUEST_KIND),
            mls_group_id: group_id,
            created_at: event.created_at,
            processed_at: Timestamp::from(124),
            content: event.content.clone(),
            tags: event.tags.clone(),
            event,
            wrapper_event_id: EventId::all_zeros(),
            epoch: None,
            state: MessageState::Processed,
        }
    }

    fn token_removal_message(
        sender: &Keys,
        group_id: GroupId,
        server_pubkey: PublicKey,
    ) -> Message {
        let fingerprint = push_token_fingerprint(NotificationPlatform::Fcm, b"firebase-token");
        let event = build_token_removal_app_event(
            sender.public_key(),
            Timestamp::from(123),
            sender.public_key(),
            NotificationPlatform::Fcm,
            &fingerprint,
            server_pubkey,
        )
        .unwrap();

        Message {
            id: event.id.unwrap_or(EventId::all_zeros()),
            pubkey: sender.public_key(),
            kind: Kind::from(TOKEN_REMOVAL_KIND),
            mls_group_id: group_id,
            created_at: event.created_at,
            processed_at: Timestamp::from(124),
            content: event.content.clone(),
            tags: event.tags.clone(),
            event,
            wrapper_event_id: EventId::all_zeros(),
            epoch: None,
            state: MessageState::Processed,
        }
    }
}

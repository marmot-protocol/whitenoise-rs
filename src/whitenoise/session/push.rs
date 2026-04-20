//! Push notification operations scoped to a single account session.
//!
//! `PushOps` is a borrow-based view that provides push registration management,
//! per-group token sharing/removal, MIP-05 message handling, and token
//! reconciliation — all without threading `account_pubkey` through every call.

use futures::stream::{self, StreamExt};
use mdk_core::mip05::{
    Mip05GroupMessage, TokenTag, build_token_removal_rumor, build_token_request_rumor,
    parse_group_message,
};
use mdk_core::prelude::{GroupId, group_types::GroupState};
use nostr_sdk::{EventId, PublicKey, RelayUrl};

use super::AccountSession;
use crate::perf_instrument;
use crate::whitenoise::accounts_groups::AccountGroup;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::push_notifications::{
    PushPlatform, PushRegistration, is_push_group_message_kind, publish_push_group_message_with,
    respond_to_token_request_with, validate_raw_token,
};

/// Maximum number of groups to publish push token events to concurrently.
const MAX_CONCURRENT_GROUP_PUBLISHES: usize = 10;

/// Account-scoped push notification operations.
pub struct PushOps<'a> {
    session: &'a AccountSession,
}

impl<'a> PushOps<'a> {
    pub(crate) fn new(session: &'a AccountSession) -> Self {
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

        let previous_token_tag = self
            .registration()
            .await?
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
            if let Err(error) = self.remove_local_token_from_joined_groups().await {
                tracing::warn!(
                    target: "whitenoise::push_notifications",
                    account = %self.session.account_pubkey.to_hex(),
                    error = %error,
                    "Failed to remove previously shared push token after registration became unshareable"
                );
            }
        } else if notifications_enabled
            && let Some(new_token_tag) = new_token_tag.as_ref()
            && let Err(error) = self.share_token_to_joined_groups(new_token_tag).await
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
        self.session.repos.push_registrations.delete().await?;

        if let Err(error) = self.remove_local_token_from_joined_groups().await {
            tracing::warn!(
                target: "whitenoise::push_notifications",
                account = %self.session.account_pubkey.to_hex(),
                error = %error,
                "Failed to remove shared push token from joined groups after local clear"
            );
        }

        Ok(())
    }

    // ── Crate-internal API ────────────────────────────────────────────

    /// Handles an incoming MIP-05 group message (token request, response, or
    /// removal).
    #[perf_instrument("push_notifications")]
    pub(crate) async fn handle_received_push_group_message(
        &self,
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
                    &message.mls_group_id,
                    message.event.pubkey,
                    leaf_index,
                    request,
                )
                .await?;

                if let Some(request_event_id) = message.event.id {
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
                self.clear_pending_response(&message.mls_group_id, &request_event_id);
            }
            Mip05GroupMessage::TokenRemoval(_) => {
                let leaf_index = sender_leaf_index.ok_or_else(|| {
                    WhitenoiseError::InvalidEvent(
                        "MIP-05 token removal missing sender leaf index".to_string(),
                    )
                })?;

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

        let Some(token_tag) = self.local_push_token_tag().await? else {
            return Ok(());
        };

        self.share_token_to_joined_groups(&token_tag).await
    }

    /// Shares the locally registered push token to a single group.
    #[perf_instrument("push_notifications")]
    pub(crate) async fn share_local_token_to_group(&self, group_id: &GroupId) -> Result<()> {
        let group_state = self
            .session
            .mdk
            .get_group(group_id)?
            .map(|g| g.state)
            .unwrap_or(GroupState::Inactive);
        if !self.is_push_gossip_eligible(group_id, group_state).await {
            return Ok(());
        }

        if !self.session.repos.settings.notifications_enabled().await? {
            return Ok(());
        }

        let Some(token_tag) = self.local_push_token_tag().await? else {
            return Ok(());
        };

        self.share_token_to_group(group_id, &token_tag).await
    }

    /// Removes the local push token from all joined active groups.
    #[perf_instrument("push_notifications")]
    pub(crate) async fn remove_local_token_from_joined_groups(&self) -> Result<()> {
        let groups = self.session.mdk.get_groups()?;

        let failures: Vec<String> = stream::iter(groups.iter())
            .map(|group| self.remove_token_from_single_group(&group.mls_group_id, group.state))
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
        let group_state = self
            .session
            .mdk
            .get_group(group_id)?
            .map(|g| g.state)
            .unwrap_or(GroupState::Inactive);

        if group_state != GroupState::Active {
            self.session
                .repos
                .group_push_tokens
                .delete_by_member_pubkey(group_id, &self.session.account_pubkey)
                .await?;
            return Ok(());
        }

        let rumor =
            build_token_removal_rumor(self.session.account_pubkey, nostr_sdk::Timestamp::now());
        if let Err(error) = publish_push_group_message_with(
            &self.session.mdk,
            self.session.group_handle.relay_control(),
            &self.session.account_pubkey,
            group_id,
            rumor,
        )
        .await
        {
            tracing::warn!(
                target: "whitenoise::push_notifications",
                account = %self.session.account_pubkey.to_hex(),
                group = %hex::encode(group_id.as_slice()),
                error = %error,
                "Failed to publish token removal to group"
            );
        }

        self.sync_local_group_push_token_cache(group_id, None).await
    }

    /// Reconciles cached group push tokens with the current active leaf map.
    #[perf_instrument("push_notifications")]
    pub(crate) async fn reconcile_group_tokens_for_active_leaves(
        &self,
        group_id: &GroupId,
    ) -> Result<()> {
        let active_leaf_map = self.session.mdk.group_leaf_map(group_id)?;
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
    async fn share_token_to_joined_groups(&self, token_tag: &TokenTag) -> Result<()> {
        let groups = self.session.mdk.get_groups()?;

        let failures: Vec<String> = stream::iter(groups.iter())
            .map(|group| async move {
                if !self
                    .is_push_gossip_eligible(&group.mls_group_id, group.state)
                    .await
                {
                    return Ok(());
                }
                self.share_token_to_group(&group.mls_group_id, token_tag)
                    .await
                    .map_err(|e| format!("{}: {e}", hex::encode(group.mls_group_id.as_slice())))
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
    async fn share_token_to_group(&self, group_id: &GroupId, token_tag: &TokenTag) -> Result<()> {
        let rumor = build_token_request_rumor(
            self.session.account_pubkey,
            nostr_sdk::Timestamp::now(),
            vec![token_tag.clone()],
        )?;
        publish_push_group_message_with(
            &self.session.mdk,
            self.session.group_handle.relay_control(),
            &self.session.account_pubkey,
            group_id,
            rumor,
        )
        .await?;
        self.sync_local_group_push_token_cache(group_id, Some(token_tag))
            .await
    }

    #[perf_instrument("push_notifications")]
    async fn remove_token_from_single_group(
        &self,
        group_id: &GroupId,
        group_state: GroupState,
    ) -> std::result::Result<(), String> {
        if group_state != GroupState::Active {
            return Ok(());
        }

        let group_id_hex = hex::encode(group_id.as_slice());
        let rumor =
            build_token_removal_rumor(self.session.account_pubkey, nostr_sdk::Timestamp::now());

        let publish_err = publish_push_group_message_with(
            &self.session.mdk,
            self.session.group_handle.relay_control(),
            &self.session.account_pubkey,
            group_id,
            rumor,
        )
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
            &self.session.database,
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

    #[perf_instrument("push_notifications")]
    async fn merge_token_request(
        &self,
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

        self.session
            .repos
            .group_push_tokens
            .upsert(
                mls_group_id,
                &member_pubkey,
                leaf_index,
                &token.server_pubkey,
                Some(&token.relay_hint),
                &token.encrypted_token.to_base64(),
            )
            .await?;

        Ok(())
    }

    #[perf_instrument("push_notifications")]
    async fn merge_token_list_response(
        &self,
        mls_group_id: &GroupId,
        response: mdk_core::mip05::TokenListResponse,
    ) -> Result<()> {
        let active_leaf_map = self.session.mdk.group_leaf_map(mls_group_id)?;

        self.session
            .repos
            .group_push_tokens
            .upsert_active_token_list_response(mls_group_id, &active_leaf_map, response.tokens)
            .await?;

        Ok(())
    }

    #[perf_instrument("push_notifications")]
    async fn respond_to_token_request(
        &self,
        group_id: &GroupId,
        request_event_id: EventId,
    ) -> Result<()> {
        respond_to_token_request_with(
            &self.session.mdk,
            &self.session.database,
            self.session.group_handle.relay_control(),
            &self.session.account_pubkey,
            group_id,
            request_event_id,
        )
        .await
    }

    #[perf_instrument("push_notifications")]
    async fn sync_local_group_push_token_cache(
        &self,
        group_id: &GroupId,
        token_tag: Option<&TokenTag>,
    ) -> Result<()> {
        let leaf_index = self.session.mdk.own_leaf_index(group_id)?;

        match token_tag {
            Some(token_tag) => {
                self.session
                    .repos
                    .group_push_tokens
                    .upsert(
                        group_id,
                        &self.session.account_pubkey,
                        leaf_index,
                        &token_tag.server_pubkey,
                        Some(&token_tag.relay_hint),
                        &token_tag.encrypted_token.to_base64(),
                    )
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

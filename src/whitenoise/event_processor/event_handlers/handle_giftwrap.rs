use std::sync::Arc;

use crate::marmot::GroupId;
use cgka_traits::transport::TransportMessage;
use nostr_sdk::prelude::*;
use transport_nostr_peeler::NostrTransportEvent;

use crate::{
    marmot::{
        publish::{MarmotMessagePublisher, MarmotPublishedEffects, publish_effects},
        transport::MarmotRelayControlPublisher,
    },
    perf_instrument, perf_span,
    whitenoise::{
        Whitenoise,
        accounts::Account,
        chat_list_streaming::ChatListUpdateTrigger,
        error::{Result, WhitenoiseError},
        group_information::GroupInformation,
        session::AccountSession,
    },
};

fn marmot_welcome_transport_message_from_event(event: &Event) -> Result<TransportMessage> {
    let transport_event = NostrTransportEvent::from_nostr_event(event).map_err(|error| {
        WhitenoiseError::InvalidEvent(format!("invalid Darkmatter welcome event: {error}"))
    })?;
    transport_event.to_transport_message().map_err(|error| {
        WhitenoiseError::InvalidEvent(format!("invalid Darkmatter welcome transport: {error}"))
    })
}

impl Whitenoise {
    #[perf_instrument("event_handlers")]
    pub async fn handle_giftwrap(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        event: Event,
    ) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::event_handlers::handle_giftwrap",
            "Giftwrap received for account: {}",
            account.pubkey.to_hex()
        );

        // Fail fast if the signer is unavailable (external-signer account
        // that hasn't re-registered after restart). The event stays
        // unprocessed and relay replay will deliver it again once the
        // signer slot is filled via register_external_signer().
        let _decrypt_span = perf_span!("event_handlers::giftwrap_decrypt");
        let signer = session
            .get_signer()
            .ok_or(WhitenoiseError::SignerUnavailable(account.pubkey))?;

        let unwrapped = extract_rumor(&signer, &event).await.map_err(|e| {
            WhitenoiseError::Configuration(format!("Failed to decrypt giftwrap: {}", e))
        })?;

        drop(_decrypt_span);

        match unwrapped.rumor.kind {
            Kind::MlsWelcome => {
                self.process_welcome(session, account, event, unwrapped.rumor)
                    .await?;
            }
            _ => {
                tracing::debug!(
                    target: "whitenoise::event_handlers::handle_giftwrap",
                    "Received unhandled giftwrap of kind {:?}",
                    unwrapped.rumor.kind
                );
            }
        }

        Ok(())
    }

    #[perf_instrument("event_handlers")]
    async fn process_welcome(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        event: Event,
        rumor: UnsignedEvent,
    ) -> Result<()> {
        // Extract key package event ID from the rumor tags early — required for pre-check
        // and key package lifecycle finalization.
        let key_package_event_id = match rumor
            .tags
            .iter()
            .find(|tag| {
                tag.kind() == TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::E))
            })
            .and_then(|tag| tag.content())
        {
            Some(content) => match EventId::parse(content) {
                Ok(event_id) => event_id,
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::event_processor::process_welcome",
                        error = %e,
                        "Malformed key package event id in Welcome e-tag, skipping"
                    );
                    return Ok(());
                }
            },
            None => {
                tracing::warn!(
                    target: "whitenoise::event_processor::process_welcome",
                    "Missing key package event id in Welcome e-tag, skipping"
                );
                return Ok(());
            }
        };

        // Pre-check: do we have this key package and is its key material still available?
        // This avoids expensive MLS crypto operations when the KP is unknown or deleted.
        let published_key_package = match session
            .repos
            .published_key_packages
            .find_by_event_id(&key_package_event_id.to_hex())
            .await
        {
            Ok(Some(pkg)) if pkg.key_material_deleted => {
                tracing::warn!(
                    target: "whitenoise::event_processor::process_welcome",
                    "Key material already deleted for this key package, skipping Welcome"
                );
                return Ok(());
            }
            Ok(None) => {
                tracing::warn!(
                    target: "whitenoise::event_processor::process_welcome",
                    "Unknown key package referenced in Welcome, skipping"
                );
                return Ok(());
            }
            Ok(Some(pkg)) => pkg, // Good — KP exists, key material available
            Err(e) => {
                tracing::error!(
                    target: "whitenoise::event_processor::process_welcome",
                    "Failed to look up key package: {}, rejecting Welcome",
                    e
                );
                return Err(e);
            }
        };

        // Reject welcomes from blocked users before any MLS processing.
        // Using `rumor.pubkey` (the Nostr identity of the sender) avoids
        // allocating protocol group state or writing any DB rows for blocked senders.
        if session.mute_list().is_user_blocked(&rumor.pubkey).await? {
            tracing::info!(
                target: "whitenoise::event_processor::process_welcome",
                "Dropping welcome from blocked user {}",
                rumor.pubkey,
            );
            return Ok(());
        }

        if published_key_package.package_version == 2 {
            self.process_marmot_welcome(session, account, &event, &rumor, key_package_event_id)
                .await?;
            return Ok(());
        }

        tracing::warn!(
            target: "whitenoise::event_processor::process_welcome",
            account = %account.pubkey,
            key_package_event = %key_package_event_id,
            package_version = published_key_package.package_version,
            "Ignoring unsupported legacy Welcome"
        );
        Ok(())
    }

    async fn process_marmot_welcome(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        event: &Event,
        rumor: &UnsignedEvent,
        key_package_event_id: EventId,
    ) -> Result<()> {
        let transport_message = marmot_welcome_transport_message_from_event(event)?;
        let marmot_session =
            session
                .marmot
                .clone()
                .ok_or(WhitenoiseError::MarmotSessionUnavailable(
                    session.account_pubkey,
                ))?;

        let joined = {
            let mut marmot = marmot_session.lock().await;
            if !marmot.recognizes_transport_message(&transport_message)? {
                return Err(WhitenoiseError::InvalidEvent(
                    "Darkmatter welcome is not addressed to this account".to_string(),
                ));
            }
            marmot.join_welcome(transport_message).await?
        };

        let routes = self
            .marmot_publish_routes_for_session_effects(session, account, &joined.effects)
            .await?;
        let publisher = MarmotRelayControlPublisher::new(&session.ephemeral, routes);
        let published = self
            .publish_marmot_welcome_effects_with_publisher(session, joined.effects, &publisher)
            .await?;

        let projection = {
            let marmot = marmot_session.lock().await;
            marmot.group_projection(&joined.group_id)?
        };
        let group_id = session
            .groups()
            .project_joined_marmot_group(&projection, None, rumor.pubkey)
            .await?;

        for event_effect in published.events {
            self.handle_marmot_group_event(session, account, event.id, event_effect)
                .await?;
        }

        self.spawn_welcome_finalization(
            session,
            account,
            group_id,
            projection.name,
            key_package_event_id,
            rumor.pubkey,
        )
        .await?;

        Ok(())
    }

    async fn publish_marmot_welcome_effects_with_publisher<P>(
        &self,
        session: &Arc<AccountSession>,
        effects: crate::marmot::session::MarmotSessionEffects,
        publisher: &P,
    ) -> Result<MarmotPublishedEffects>
    where
        P: MarmotMessagePublisher + Sync + ?Sized,
    {
        let marmot_session =
            session
                .marmot
                .clone()
                .ok_or(WhitenoiseError::MarmotSessionUnavailable(
                    session.account_pubkey,
                ))?;
        let published = publish_effects(marmot_session, publisher, effects).await?;
        Self::ensure_marmot_inbound_publish_succeeded(&published)?;
        if !published.queued.is_empty() {
            return Err(WhitenoiseError::Internal(format!(
                "Darkmatter welcome queued {} follow-up intent(s); queued intent driver is not implemented",
                published.queued.len()
            )));
        }

        Ok(published)
    }

    async fn spawn_welcome_finalization(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        group_id: GroupId,
        group_name: String,
        key_package_event_id: EventId,
        welcomer_pubkey: PublicKey,
    ) -> Result<()> {
        // Spawn background task for remaining operations (DB writes, network calls).
        // All operations are idempotent and failures are logged but don't stop other operations.
        //
        // Capture the `Arc<AccountSession>` here rather than re-resolving it
        // inside the spawned task. If the user logs out between Welcome
        // acceptance and the background rotation step, the session would
        // disappear from the registry and `mark_consumed()` would silently
        // skip — leaving the per-account `published_key_packages` row
        // without `consumed_at`, which the maintenance task uses to decide
        // when to delete key material.
        let tid = crate::perf::current_trace_id();
        let account_owned = account.clone();
        let session_owned = session.clone();
        let whitenoise = self.arc()?;
        self.spawn_background(crate::perf::with_trace_id(tid, async move {
            Self::background_finalize_welcome(
                whitenoise,
                account_owned,
                session_owned,
                group_id,
                group_name,
                key_package_event_id,
                welcomer_pubkey,
            )
            .await;
        }))
        .await;

        Ok(())
    }

    /// Background task wrapper that holds the `Whitenoise` `Arc` alive while the
    /// welcome-finalization core logic runs.
    #[perf_instrument("event_handlers")]
    async fn background_finalize_welcome(
        whitenoise: Arc<Whitenoise>,
        account: Account,
        session: Arc<AccountSession>,
        group_id: GroupId,
        group_name: String,
        key_package_event_id: EventId,
        welcomer_pubkey: PublicKey,
    ) {
        let Some(_session_operation) = session.begin_operation() else {
            tracing::debug!(
                target: "whitenoise::event_processor::process_welcome::background",
                account = %account.pubkey.to_hex(),
                group = %hex::encode(group_id.as_slice()),
                "Skipping welcome finalization because account session is closing"
            );
            return;
        };

        Self::finalize_welcome_with_instance(
            &whitenoise,
            &account,
            &session,
            &group_id,
            &group_name,
            key_package_event_id,
            welcomer_pubkey,
        )
        .await;
    }

    /// Core welcome finalization logic. Testable because it takes Whitenoise as a parameter.
    /// Handles DB writes, network calls, and other non-critical operations.
    /// All operations are idempotent and failures are logged but don't stop other operations.
    ///
    /// # Sequencing
    ///
    /// 1. **Subscription setup** — awaited first so the relay connection is live
    ///    before catch-up or background reconciliation starts.
    /// 2. **Independent ops** (group info, key rotation, image sync, welcomer
    ///    user lookup) — run concurrently; failures are logged but do not block.
    #[perf_instrument("event_handlers")]
    pub(crate) async fn finalize_welcome_with_instance(
        whitenoise: &Whitenoise,
        account: &Account,
        session: &Arc<AccountSession>,
        group_id: &GroupId,
        group_name: &str,
        key_package_event_id: EventId,
        welcomer_pubkey: PublicKey,
    ) {
        // --- Step 1: subscription setup (must happen before catch-up) ---
        //
        // Uses sync_group_subscriptions which only updates the group plane.
        // Unlike refresh_account_subscriptions, this does NOT tear down the
        // inbox or existing group subscriptions — avoiding a cascade failure
        // when relay connections are flaky (e.g. mobile resuming from background).
        match whitenoise.sync_group_subscriptions(account).await {
            Ok(()) => {
                tracing::debug!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    account = %account.pubkey.to_hex(),
                    "Group subscriptions established"
                );
            }
            Err(e) => {
                tracing::error!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    account = %account.pubkey.to_hex(),
                    group = %hex::encode(group_id.as_slice()),
                    error = %e,
                    reason = "subscription_setup_failed",
                    "Subscription setup failed; skipping catch-up"
                );
            }
        }

        // --- Step 2: independent operations (run concurrently regardless of subscription status) ---
        let (group_info_result, key_rotation_result, image_sync_result, welcomer_user_result) = tokio::join!(
            Self::create_group_info(whitenoise, group_id, group_name),
            Self::rotate_key_package(session, key_package_event_id),
            Self::sync_group_image(session, group_id),
            Self::ensure_welcomer_user_exists(whitenoise, welcomer_pubkey),
        );

        if let Err(e) = group_info_result {
            tracing::error!(
                target: "whitenoise::event_processor::process_welcome::background",
                group = %hex::encode(group_id.as_slice()),
                error = %e,
                "Failed to create GroupInformation"
            );
        } else {
            whitenoise
                .emit_chat_list_update(account, group_id, ChatListUpdateTrigger::NewGroup)
                .await;
            whitenoise
                .emit_group_invite_notification_if_enabled(
                    account,
                    group_id,
                    group_name,
                    welcomer_pubkey,
                )
                .await;
        }

        if let Err(e) = key_rotation_result {
            tracing::error!(
                target: "whitenoise::event_processor::process_welcome::background",
                account = %account.pubkey.to_hex(),
                error = %e,
                "Failed to rotate key package"
            );
        }

        if let Err(e) = image_sync_result {
            tracing::warn!(
                target: "whitenoise::event_processor::process_welcome::background",
                error = %e,
                "Failed to sync group image cache"
            );
        }

        if let Err(e) = welcomer_user_result {
            tracing::error!(
                target: "whitenoise::event_processor::process_welcome::background",
                account = %account.pubkey.to_hex(),
                error = %e,
                "Failed to ensure welcomer user exists"
            );
        }

        // Push token sharing is deferred until the user explicitly accepts
        // the group invite (see accept_account_group). This prevents leaking
        // the device's push token into pending or declined groups.

        tracing::debug!(
            target: "whitenoise::event_processor::process_welcome::background",
            account = %account.pubkey.to_hex(),
            group = %hex::encode(group_id.as_slice()),
            "Completed post-welcome processing"
        );
    }

    #[perf_instrument("event_handlers")]
    async fn create_group_info(
        whitenoise: &Whitenoise,
        group_id: &GroupId,
        group_name: &str,
    ) -> Result<()> {
        GroupInformation::create_for_group(whitenoise, group_id, None, group_name).await?;
        Ok(())
    }

    /// Handle key package rotation after welcome.
    ///
    /// Marks the consumed key package in the published_key_packages table,
    /// then deletes it from relays and publishes a fresh replacement.
    #[perf_instrument("event_handlers")]
    async fn rotate_key_package(
        session: &Arc<AccountSession>,
        key_package_event_id: EventId,
    ) -> Result<()> {
        // Mark the key package as consumed so the maintenance task knows
        // to clean up local key material after the quiet period. Uses the
        // captured session reference so logout between Welcome accept and
        // this rotation cannot drop the consumed-at write.
        if let Err(e) = session
            .repos
            .published_key_packages
            .mark_consumed(&key_package_event_id.to_hex())
            .await
        {
            tracing::warn!(
                target: "whitenoise::event_processor::process_welcome::background",
                "Failed to mark key package as consumed: {}",
                e
            );
        }

        // Publish new key package first so the account is never left with zero
        // key packages on relays. If this fails, the old one stays available.
        session.key_packages().publish().await?;
        tracing::debug!(
            target: "whitenoise::event_processor::process_welcome::background",
            "Published new key package"
        );

        // Now delete the used key package. Failure here is non-fatal — the
        // scheduler will clean it up during routine maintenance.
        match session
            .key_packages()
            .delete(&key_package_event_id, false)
            .await
        {
            Ok(true) => {
                tracing::debug!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    "Deleted used key package from relays"
                );
            }
            Ok(false) => {
                tracing::debug!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    "Key package already deleted, skipping"
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    "Failed to delete used key package, scheduler will clean up: {}",
                    e
                );
            }
        }

        Ok(())
    }

    /// Sync group image cache if needed
    #[perf_instrument("event_handlers")]
    async fn sync_group_image(session: &Arc<AccountSession>, group_id: &GroupId) -> Result<()> {
        session
            .groups()
            .media()
            .sync_group_image_cache_if_needed(group_id)
            .await
    }

    #[perf_instrument("event_handlers")]
    async fn ensure_welcomer_user_exists(
        whitenoise: &Whitenoise,
        welcomer_pubkey: PublicKey,
    ) -> Result<()> {
        whitenoise.resolve_user(&welcomer_pubkey).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    use crate::marmot::GroupConfig;
    use crate::marmot::publish::{MarmotMessagePublisher, MarmotPublishOutcome};
    use crate::whitenoise::accounts_groups::AccountGroup;
    use crate::whitenoise::key_packages::MLS_KEY_PACKAGE_KIND;
    use crate::whitenoise::test_utils::*;

    fn synthetic_welcome_rumor(creator_pubkey: PublicKey, tags: Vec<Tag>) -> UnsignedEvent {
        let mut rumor = UnsignedEvent::new(
            creator_pubkey,
            Timestamp::now(),
            Kind::MlsWelcome,
            tags,
            "unsupported legacy welcome".to_string(),
        );
        rumor.ensure_id();
        rumor
    }

    async fn build_welcome_giftwrap_from_rumor(
        whitenoise: &Whitenoise,
        creator_account: &Account,
        member_account: &Account,
        rumor: UnsignedEvent,
    ) -> Event {
        let creator_signer = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&creator_account.pubkey)
            .unwrap();

        EventBuilder::gift_wrap(&creator_signer, &member_account.pubkey, rumor, vec![])
            .await
            .unwrap()
    }

    async fn create_legacy_key_package_row(session: &Arc<AccountSession>, event_id: EventId) {
        session
            .repos
            .published_key_packages
            .create(
                b"legacy-test-key-package",
                &event_id.to_hex(),
                MLS_KEY_PACKAGE_KIND,
                Some("legacy-test-d"),
            )
            .await
            .unwrap();
    }

    fn event_tag(event_id: EventId) -> Tag {
        Tag::parse(vec!["e", &event_id.to_hex()]).unwrap()
    }

    fn deterministic_event_id(seed: u64) -> EventId {
        EventId::from_hex(&format!("{seed:064x}")).unwrap()
    }

    fn assert_obsolete_mls_artifacts_absent_for_account(
        whitenoise: &Whitenoise,
        account: &Account,
    ) {
        let artifacts = ObsoleteMlsArtifacts {
            storage_path: crate::whitenoise::test_utils::obsolete_mls_storage_path(
                &account.pubkey,
                &whitenoise.config().data_dir,
            ),
        };
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn test_handle_giftwrap_welcome_missing_e_tag_skipped() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();

        let mut welcome_rumor = synthetic_welcome_rumor(
            creator_account.pubkey,
            vec![event_tag(deterministic_event_id(0x6001))],
        );
        assert_obsolete_mls_artifacts_absent_for_account(&whitenoise, &member_account);
        let mut tags_without_e = Tags::new();
        for tag in welcome_rumor.tags.iter() {
            if tag.kind() != TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::E)) {
                tags_without_e.push(tag.clone());
            }
        }
        welcome_rumor.tags = tags_without_e;
        welcome_rumor.ensure_id();

        let creator_signer = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&creator_account.pubkey)
            .unwrap();
        let giftwrap_event = EventBuilder::gift_wrap(
            &creator_signer,
            &member_account.pubkey,
            welcome_rumor,
            vec![],
        )
        .await
        .unwrap();

        let result = whitenoise
            .handle_giftwrap(&member_session, &member_account, giftwrap_event)
            .await;
        assert!(
            result.is_ok(),
            "Missing e-tag Welcome should be skipped safely"
        );

        assert_obsolete_mls_artifacts_absent_for_account(&whitenoise, &member_account);
    }

    #[tokio::test]
    async fn test_handle_giftwrap_welcome_malformed_e_tag_skipped() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();

        let mut welcome_rumor = synthetic_welcome_rumor(
            creator_account.pubkey,
            vec![event_tag(deterministic_event_id(0x6002))],
        );
        assert_obsolete_mls_artifacts_absent_for_account(&whitenoise, &member_account);
        let mut malformed_tags = Tags::new();
        for tag in welcome_rumor.tags.iter() {
            if tag.kind() != TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::E)) {
                malformed_tags.push(tag.clone());
            }
        }
        malformed_tags.push(Tag::parse(vec!["e", "not-an-event-id"]).unwrap());
        welcome_rumor.tags = malformed_tags;
        welcome_rumor.ensure_id();

        let creator_signer = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&creator_account.pubkey)
            .unwrap();
        let giftwrap_event = EventBuilder::gift_wrap(
            &creator_signer,
            &member_account.pubkey,
            welcome_rumor,
            vec![],
        )
        .await
        .unwrap();

        let result = whitenoise
            .handle_giftwrap(&member_session, &member_account, giftwrap_event)
            .await;
        assert!(
            result.is_ok(),
            "Malformed e-tag Welcome should be skipped safely"
        );

        assert_obsolete_mls_artifacts_absent_for_account(&whitenoise, &member_account);
    }

    #[tokio::test]
    async fn test_handle_giftwrap_legacy_welcome_does_not_create_legacy_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();

        let key_package_event_id = deterministic_event_id(0x6003);
        create_legacy_key_package_row(&member_session, key_package_event_id).await;
        let welcome_rumor = synthetic_welcome_rumor(
            creator_account.pubkey,
            vec![event_tag(key_package_event_id)],
        );
        let giftwrap_event = build_welcome_giftwrap_from_rumor(
            &whitenoise,
            &creator_account,
            &member_account,
            welcome_rumor,
        )
        .await;
        assert_obsolete_mls_artifacts_absent_for_account(&whitenoise, &member_account);

        whitenoise
            .handle_giftwrap(&member_session, &member_account, giftwrap_event)
            .await
            .unwrap();

        assert_obsolete_mls_artifacts_absent_for_account(&whitenoise, &member_account);
        assert!(
            AccountGroup::visible_for_account(&whitenoise, &member_account.pubkey)
                .await
                .unwrap()
                .is_empty(),
            "unsupported legacy welcomes must not create account group rows"
        );
    }

    #[tokio::test]
    async fn test_handle_giftwrap_creates_account_group_synchronously() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let (group_id, giftwrap_event) = build_darkmatter_welcome_giftwrap(
            &whitenoise,
            &creator_account,
            &member_account,
            "darkmatter synchronous welcome",
        )
        .await;

        // Member processes the welcome
        let result = whitenoise
            .handle_giftwrap(&member_session, &member_account, giftwrap_event)
            .await;
        assert!(result.is_ok());

        // CRITICAL: AccountGroup must exist immediately after handle_giftwrap returns
        // (not just after background task completes). This prevents race conditions
        // where Flutter polls groups() before the AccountGroup record exists.
        let session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let account_group = AccountGroup::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            &session.account_db.inner.pool,
        )
        .await
        .unwrap();

        assert!(
            account_group.is_some(),
            "AccountGroup must exist synchronously after handle_giftwrap"
        );

        let ag = account_group.unwrap();
        assert!(
            ag.is_pending(),
            "AccountGroup should be pending (user_confirmation = NULL)"
        );

        // Verify welcomer_pubkey is set to the creator's pubkey
        assert_eq!(
            ag.welcomer_pubkey,
            Some(creator_account.pubkey),
            "AccountGroup.welcomer_pubkey should be the group creator's pubkey"
        );
    }

    #[derive(Clone, Default)]
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

    async fn build_darkmatter_welcome_giftwrap(
        whitenoise: &Whitenoise,
        creator_account: &Account,
        member_account: &Account,
        group_name: &str,
    ) -> (GroupId, Event) {
        wait_for_key_package_publication(whitenoise, &[member_account]).await;

        let config = GroupConfig::new(
            group_name.to_string(),
            "joined from a Darkmatter welcome".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("ws://localhost:8080/").unwrap()],
            vec![creator_account.pubkey],
            None,
        );
        let publisher = CapturingMarmotPublisher::default();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let created_group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member_account.pubkey],
                config,
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

        (created_group.mls_group_id, giftwrap_event)
    }

    #[tokio::test]
    async fn test_handle_giftwrap_accepts_darkmatter_welcome_without_legacy_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        let (created_group_id, giftwrap_event) = build_darkmatter_welcome_giftwrap(
            &whitenoise,
            &creator_account,
            &member_account,
            "darkmatter welcome",
        )
        .await;

        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        assert_obsolete_mls_artifacts_absent_for_account(&whitenoise, &member_account);
        whitenoise
            .handle_giftwrap(&member_session, &member_account, giftwrap_event)
            .await
            .unwrap();

        let member_groups = member_session.groups().visible().await.unwrap();
        assert!(
            member_groups
                .iter()
                .any(|group| group.group.mls_group_id == created_group_id),
            "Darkmatter welcome should create a visible pending group projection"
        );
        let account_group = AccountGroup::find_by_account_and_group(
            &member_account.pubkey,
            &created_group_id,
            &member_session.account_db.inner.pool,
        )
        .await
        .unwrap()
        .expect("Darkmatter welcome should create AccountGroup");
        assert!(account_group.is_pending());
        assert_eq!(account_group.welcomer_pubkey, Some(creator_account.pubkey));

        assert_obsolete_mls_artifacts_absent_for_account(&whitenoise, &member_account);
    }

    #[tokio::test]
    async fn test_darkmatter_welcome_effect_publish_uses_injected_publisher() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let config = GroupConfig::new(
            "darkmatter welcome effects".to_string(),
            "joined from a Darkmatter welcome".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("ws://localhost:8080/").unwrap()],
            vec![creator_account.pubkey],
            None,
        );
        let group_publisher = CapturingMarmotPublisher::default();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member_account.pubkey],
                config,
                None,
                &group_publisher,
            )
            .await
            .unwrap();

        let welcome_message = group_publisher
            .messages()
            .into_iter()
            .find(|message| {
                matches!(
                    message.envelope,
                    cgka_traits::TransportEnvelope::Welcome { .. }
                )
            })
            .expect("Darkmatter group creation should publish a welcome");
        let effects = crate::marmot::session::MarmotSessionEffects {
            events: Vec::new(),
            publish: vec![crate::marmot::session::PublishWork::ApplicationMessage {
                msg: welcome_message.clone(),
            }],
            queued: Vec::new(),
        };
        let effect_publisher = CapturingMarmotPublisher::default();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();

        let published = whitenoise
            .publish_marmot_welcome_effects_with_publisher(
                &member_session,
                effects,
                &effect_publisher,
            )
            .await
            .unwrap();

        assert!(published.failures.is_empty());
        assert_eq!(effect_publisher.messages().len(), 1);
        assert_eq!(effect_publisher.messages()[0].id, welcome_message.id);
    }

    #[tokio::test]
    async fn test_handle_giftwrap_non_welcome_ok() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let account_session = whitenoise.require_session(&account.pubkey).unwrap();

        // Build a non-welcome rumor and giftwrap it to the account
        let sender_keys = create_test_keys();
        let mut rumor = UnsignedEvent::new(
            sender_keys.public_key(), // Use sender's pubkey (must match seal signer)
            Timestamp::now(),
            Kind::TextNote,
            vec![],
            "not a welcome".to_string(),
        );
        rumor.ensure_id();

        let giftwrap_event = EventBuilder::gift_wrap(&sender_keys, &account.pubkey, rumor, vec![])
            .await
            .unwrap();

        let result = whitenoise
            .handle_giftwrap(&account_session, &account, giftwrap_event)
            .await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);
    }

    #[tokio::test]
    async fn test_sync_group_subscriptions_no_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // New account has no groups — sync should succeed as a no-op
        let result = whitenoise.sync_group_subscriptions(&account).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_sync_group_subscriptions_with_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;
        wait_for_key_package_publication(&whitenoise, &[&members[0].0]).await;

        let config = create_group_config(vec![creator_account.pubkey]);
        whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member_pubkey], config, None)
            .await
            .unwrap();

        // Creator should have one group — sync should update the group plane
        let result = whitenoise.sync_group_subscriptions(&creator_account).await;
        assert!(result.is_ok());

        // Group plane should contain the group
        let plane_count = whitenoise
            .shared
            .relay_control
            .group_plane_account_group_count(&creator_account.pubkey)
            .await;
        assert_eq!(plane_count, 1, "Group plane should have one group");
    }

    #[tokio::test]
    async fn test_finalize_welcome_with_instance_completes() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[42; 32]);
        let group_name = "Test Group";
        let welcomer_pubkey = whitenoise.create_identity().await.unwrap().pubkey;

        // Pre-create AccountGroup to simulate synchronous creation in process_welcome
        AccountGroup::get_or_create(&whitenoise, &account.pubkey, &group_id, None)
            .await
            .unwrap();

        let session = whitenoise.session(&account.pubkey).unwrap();
        // Run finalize_welcome_with_instance - it should complete without panic
        // Some operations may fail (e.g., group not in MLS) but the function handles errors gracefully
        Whitenoise::finalize_welcome_with_instance(
            &whitenoise,
            &account,
            &session,
            &group_id,
            group_name,
            EventId::all_zeros(),
            welcomer_pubkey,
        )
        .await;

        // Verify AccountGroup still exists and is pending
        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let account_group = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &session.account_db.inner.pool,
        )
        .await
        .unwrap();
        assert!(account_group.is_some(), "AccountGroup should exist");
        assert!(
            account_group.unwrap().is_pending(),
            "AccountGroup should still be pending"
        );
    }

    #[tokio::test]
    async fn test_finalize_welcome_with_instance_idempotent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[44; 32]);
        let group_name = "Idempotent Test Group";
        let welcomer_pubkey = whitenoise.create_identity().await.unwrap().pubkey;

        // Pre-create AccountGroup to simulate synchronous creation in process_welcome
        AccountGroup::get_or_create(&whitenoise, &account.pubkey, &group_id, None)
            .await
            .unwrap();

        let session = whitenoise.session(&account.pubkey).unwrap();
        // Run twice - should not panic
        Whitenoise::finalize_welcome_with_instance(
            &whitenoise,
            &account,
            &session,
            &group_id,
            group_name,
            EventId::all_zeros(),
            welcomer_pubkey,
        )
        .await;

        Whitenoise::finalize_welcome_with_instance(
            &whitenoise,
            &account,
            &session,
            &group_id,
            group_name,
            EventId::all_zeros(),
            welcomer_pubkey,
        )
        .await;

        // Should still have exactly one AccountGroup
        let visible = AccountGroup::visible_for_account(&whitenoise, &account.pubkey)
            .await
            .unwrap();
        let matching: Vec<_> = visible
            .iter()
            .filter(|ag| ag.mls_group_id == group_id)
            .collect();
        assert_eq!(matching.len(), 1, "Should have exactly one AccountGroup");
    }

    #[tokio::test]
    async fn test_finalize_welcome_does_not_advance_epoch() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let (group_id, giftwrap_event) = build_darkmatter_welcome_giftwrap(
            &whitenoise,
            &creator_account,
            &member_account,
            "darkmatter epoch welcome",
        )
        .await;
        whitenoise
            .handle_giftwrap(&member_session, &member_account, giftwrap_event)
            .await
            .unwrap();
        whitenoise.wait_for_pending_background_tasks().await;

        let group = member_session.groups().get(&group_id).unwrap();
        let epoch_after_welcome = group.epoch;

        // Run finalize_welcome_with_instance.
        Whitenoise::finalize_welcome_with_instance(
            &whitenoise,
            &member_account,
            &member_session,
            &group_id,
            &group.name,
            EventId::all_zeros(),
            creator_account.pubkey,
        )
        .await;

        // Re-read the group and verify finalization does not advance the epoch.
        let updated_group = member_session.groups().get(&group_id).unwrap();
        assert_eq!(
            updated_group.epoch, epoch_after_welcome,
            "Epoch should remain unchanged during welcome finalization (was {}, now {})",
            epoch_after_welcome, updated_group.epoch
        );
    }

    /// After welcome finalization, the new group must appear in the group
    /// plane and the account's subscriptions must remain operational.
    /// This is the core regression test for the background subscription bug:
    /// the old code tore down all subscriptions (inbox + groups) and rebuilt
    /// from scratch, which could cascade-fail on mobile. The fix uses an
    /// incremental group plane update that leaves the inbox untouched.
    #[tokio::test]
    async fn test_darkmatter_welcome_finalization_adds_group_to_plane() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();

        // Member starts with subscriptions but no groups
        assert!(
            whitenoise
                .is_account_subscriptions_operational(&member_account)
                .await
                .unwrap(),
            "Member should be operational before welcome"
        );
        assert_eq!(
            whitenoise
                .shared
                .relay_control
                .group_plane_account_group_count(&member_account.pubkey)
                .await,
            0,
            "Member should have 0 groups in plane before welcome"
        );

        let (_group_id, giftwrap_event) = build_darkmatter_welcome_giftwrap(
            &whitenoise,
            &creator_account,
            &member_account,
            "darkmatter plane welcome",
        )
        .await;
        whitenoise
            .handle_giftwrap(&member_session, &member_account, giftwrap_event)
            .await
            .unwrap();
        whitenoise.wait_for_pending_background_tasks().await;

        // The new group must be in the group plane
        let plane_count = whitenoise
            .shared
            .relay_control
            .group_plane_account_group_count(&member_account.pubkey)
            .await;
        assert_eq!(
            plane_count, 1,
            "Group plane should have 1 group after welcome finalization"
        );

        // Subscriptions must still be operational (inbox survived)
        assert!(
            whitenoise
                .is_account_subscriptions_operational(&member_account)
                .await
                .unwrap(),
            "Subscriptions should remain operational after welcome finalization"
        );
    }

    /// When the account's signing key is not in the secrets store,
    /// `finalize_welcome_with_instance` must complete without panicking.
    /// Operations that need a signer (e.g. key rotation) fail gracefully
    /// while others (subscription sync, group info) proceed normally.
    #[tokio::test]
    async fn test_finalize_welcome_no_signer_completes_gracefully() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[99; 32]);
        let welcomer_pubkey = whitenoise.create_identity().await.unwrap().pubkey;

        // Pre-create AccountGroup so the function has a record to work with
        AccountGroup::get_or_create(&whitenoise, &account.pubkey, &group_id, None)
            .await
            .unwrap();

        // Remove the private key so signer-dependent operations fail
        whitenoise
            .shared
            .secrets_store
            .remove_private_key_for_pubkey(&account.pubkey)
            .unwrap();

        let session = whitenoise.session(&account.pubkey).unwrap();
        // Should complete without panic despite missing signer
        Whitenoise::finalize_welcome_with_instance(
            &whitenoise,
            &account,
            &session,
            &group_id,
            "Test Group",
            EventId::all_zeros(),
            welcomer_pubkey,
        )
        .await;

        // AccountGroup must still exist
        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let ag = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &session.account_db.inner.pool,
        )
        .await
        .unwrap();
        assert!(ag.is_some(), "AccountGroup must survive missing signer");
    }

    /// When the DB lookup for the key package fails (e.g. table missing),
    /// `handle_giftwrap` must return an error so the upstream retry logic kicks in.
    /// This covers the `Err(e) => return Err(e.into())` path in `process_welcome`.
    #[tokio::test]
    async fn test_handle_giftwrap_welcome_db_error_rejects() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();

        let welcome_rumor = synthetic_welcome_rumor(
            creator_account.pubkey,
            vec![event_tag(deterministic_event_id(0x6004))],
        );
        let giftwrap_event = build_welcome_giftwrap_from_rumor(
            &whitenoise,
            &creator_account,
            &member_account,
            welcome_rumor,
        )
        .await;

        // Corrupt the per-account database by dropping the published_key_packages
        // table. The table lives in the account DB after the 18c split.
        sqlx::query("DROP TABLE published_key_packages")
            .execute(&member_session.account_db.inner.pool)
            .await
            .unwrap();

        // Processing should fail because the DB lookup errors out
        let result = whitenoise
            .handle_giftwrap(&member_session, &member_account, giftwrap_event)
            .await;
        assert!(
            result.is_err(),
            "Welcome with broken DB should return an error, got Ok"
        );
    }
}

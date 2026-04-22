//! Group read and mutation operations scoped to an [`AccountSession`].

mod media;

pub use self::media::MediaOps;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use futures::future::{join_all, try_join_all};
use mdk_core::prelude::*;
use nostr_sdk::prelude::*;

use super::AccountSession;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::accounts_groups::AccountGroup;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::group_information::{GroupInformation, GroupType};
use crate::whitenoise::groups::{GroupWithInfoAndMembership, GroupWithMembership};
use crate::whitenoise::key_packages::validate_fetched_member_key_package;
use crate::whitenoise::relays::{Relay, RelayType};
use crate::whitenoise::users::User;

/// View over [`AccountSession`] for group operations.
///
/// Obtain via [`AccountSession::groups`].
pub struct GroupOps<'a> {
    session: &'a AccountSession,
}

impl<'a> GroupOps<'a> {
    pub(super) fn new(session: &'a AccountSession) -> Self {
        Self { session }
    }

    // ── Singleton bridge ──────────────────────────────────────────────

    /// Obtain a `&'static Whitenoise` reference via the global singleton.
    ///
    /// Several mutation methods need access to relay control, user relay
    /// lookups, and account-group records that still live on `Whitenoise`.
    /// This bridge keeps the session API functional while ownership migrates.
    // TODO(phase-16): Remove singleton bridge when relay_control moves to session.
    fn wn() -> Result<&'static Whitenoise> {
        Whitenoise::get_instance()
            .map_err(|_| WhitenoiseError::Internal("Whitenoise singleton unavailable".to_string()))
    }

    // ── Read operations ───────────────────────────────────────────────

    /// Return all groups for this account.
    ///
    /// When `active_filter` is `true`, only groups in `Active` state are
    /// returned. When `false`, all groups (including inactive) are included.
    pub fn all(&self, active_filter: bool) -> Result<Vec<group_types::Group>> {
        let groups: Vec<group_types::Group> = self
            .session
            .mdk
            .get_groups()
            .map_err(WhitenoiseError::from)?
            .into_iter()
            .filter(|group| !active_filter || group.state == group_types::GroupState::Active)
            .collect();

        Ok(groups)
    }

    /// Return visible groups for this account (pending + accepted + removed,
    /// excluding declined).
    ///
    /// `AccountGroup` is the source of truth for visibility:
    /// - **Pending** (`user_confirmation = NULL`) — invited, not yet accepted
    /// - **Accepted** (`user_confirmation = true`) — active member
    /// - **Removed** (`user_confirmation = true`, `removed_at IS NOT NULL`) —
    ///   kicked by admin; stays visible until archived
    /// - **Declined** (`user_confirmation = false`) — hidden, never shown
    ///
    /// All MDK groups (including inactive) are fetched so that removed groups,
    /// which MDK marks as `Inactive`, are still paired with their
    /// `AccountGroup` records.
    pub async fn visible(&self) -> Result<Vec<GroupWithMembership>> {
        let all_groups = self.all(false)?;

        let visible_account_groups = AccountGroup::find_visible_for_account(
            &self.session.account_pubkey,
            &self.session.database,
        )
        .await?;

        let memberships_by_id: HashMap<_, _> = visible_account_groups
            .into_iter()
            .map(|ag| (ag.mls_group_id.clone(), ag))
            .collect();

        Ok(all_groups
            .into_iter()
            .filter_map(|group| {
                let membership = memberships_by_id.get(&group.mls_group_id)?.clone();
                if group.state == group_types::GroupState::Active || membership.is_removed() {
                    Some(GroupWithMembership { group, membership })
                } else {
                    None
                }
            })
            .collect())
    }

    /// Return visible groups paired with their [`GroupInformation`].
    ///
    /// Eliminates the N+1 pattern of calling [`Self::visible`] then fetching
    /// info individually. Groups with no `group_information` row are excluded.
    pub async fn visible_with_info(&self) -> Result<Vec<GroupWithInfoAndMembership>> {
        let visible = self.visible().await?;

        if visible.is_empty() {
            return Ok(Vec::new());
        }

        let group_ids: Vec<_> = visible
            .iter()
            .map(|gwm| gwm.group.mls_group_id.clone())
            .collect();
        let info_list =
            GroupInformation::find_by_mls_group_ids(&group_ids, &self.session.database).await?;
        let info_by_id: HashMap<_, _> = info_list
            .into_iter()
            .map(|gi| (gi.mls_group_id.clone(), gi))
            .collect();

        let result = visible
            .into_iter()
            .filter_map(|gwm| {
                let info = info_by_id.get(&gwm.group.mls_group_id)?.clone();
                Some(GroupWithInfoAndMembership {
                    group: gwm.group,
                    info,
                    membership: gwm.membership,
                })
            })
            .collect();

        Ok(result)
    }

    /// Retrieve a single group by its MLS group ID.
    pub fn get(&self, group_id: &GroupId) -> Result<group_types::Group> {
        self.session
            .mdk
            .get_group(group_id)
            .map_err(WhitenoiseError::from)?
            .ok_or(WhitenoiseError::GroupNotFound)
    }

    /// Return the members of a group.
    pub fn members(&self, group_id: &GroupId) -> Result<Vec<PublicKey>> {
        Ok(self
            .session
            .mdk
            .get_members(group_id)
            .map_err(WhitenoiseError::from)?
            .into_iter()
            .collect())
    }

    /// Return the relay URLs for a group.
    pub fn relays(&self, group_id: &GroupId) -> Result<BTreeSet<RelayUrl>> {
        self.session
            .mdk
            .get_relays(group_id)
            .map_err(WhitenoiseError::from)
    }

    /// Return the admin public keys for a group.
    pub fn admins(&self, group_id: &GroupId) -> Result<Vec<PublicKey>> {
        Ok(self.get(group_id)?.admin_pubkeys.into_iter().collect())
    }

    // ── Mutation helpers ──────────────────────────────────────────────

    /// Verify the current account is an admin of the given group.
    fn ensure_admin(&self, group_id: &GroupId) -> Result<()> {
        let admins = self.admins(group_id)?;
        if !admins.contains(&self.session.account_pubkey) {
            return Err(WhitenoiseError::AccountNotAuthorized);
        }
        Ok(())
    }

    /// Return the relay URLs configured for the group, or error if none.
    pub(crate) fn ensure_relays(&self, group_id: &GroupId) -> Result<Vec<RelayUrl>> {
        let relays = self
            .session
            .mdk
            .get_relays(group_id)
            .map_err(WhitenoiseError::from)?;
        if relays.is_empty() {
            return Err(WhitenoiseError::GroupMissingRelays);
        }
        Ok(relays.into_iter().collect())
    }

    /// Publish an evolution event and merge the pending commit on success.
    ///
    /// Per MIP-03 this is the canonical ordering for MLS state evolution:
    /// 1. Caller creates the pending commit via an MDK operation
    /// 2. This method publishes the evolution event (with retry)
    /// 3. Only after at least one relay accepts, the pending commit is merged
    ///
    /// If all publish attempts fail, the pending commit is cleared via
    /// `clear_pending_commit`, rolling back the MLS group to its pre-commit
    /// state.
    // TODO(phase-16): Remove singleton bridge when relay_control moves to session.
    pub(crate) async fn publish_and_merge_commit(
        &self,
        evolution_event: Event,
        group_id: &GroupId,
        relay_urls: &[RelayUrl],
    ) -> Result<()> {
        let wn = Self::wn()?;
        if let Err(publish_err) = wn
            .publish_event_with_retry(evolution_event, &self.session.account_pubkey, relay_urls)
            .await
        {
            if let Err(clear_err) = self.session.mdk.clear_pending_commit(group_id) {
                tracing::warn!(
                    target: "whitenoise::session::groups",
                    "Failed to clear pending commit after publish failure for group {}: {}",
                    hex::encode(group_id.as_slice()),
                    clear_err,
                );
            }
            return Err(publish_err);
        }
        self.session.mdk.merge_pending_commit(group_id)?;
        Ok(())
    }

    // ── Mutation operations ───────────────────────────────────────────

    /// Adds new members to an existing MLS group.
    ///
    /// Performs the complete workflow: fetch key packages, add members via MDK,
    /// publish evolution event, merge commit, and send welcome messages.
    // TODO(phase-16): Remove singleton bridge when relay_control moves to session.
    pub async fn add_members(
        &self,
        group_id: &GroupId,
        member_pubkeys: Vec<PublicKey>,
    ) -> Result<()> {
        self.ensure_admin(group_id)?;

        let wn = Self::wn()?;
        let signer = self
            .session
            .get_signer()
            .ok_or(WhitenoiseError::SignerUnavailable(
                self.session.account_pubkey,
            ))?;
        let account =
            Account::find_by_pubkey(&self.session.account_pubkey, &wn.shared.database).await?;

        let mut key_package_events: Vec<Event> = Vec::new();
        let mut users = Vec::new();

        for pk in member_pubkeys.iter() {
            let (user, newly_created) =
                User::find_or_create_by_pubkey(pk, &wn.shared.database).await?;

            if newly_created && let Err(e) = user.update_relay_lists(wn).await {
                tracing::warn!(
                    target: "whitenoise::session::groups",
                    "Failed to update relay lists for new user {}: {}",
                    user.pubkey,
                    e
                );
            }

            let mut relays_to_use = user
                .relays(RelayType::KeyPackage, &wn.shared.database)
                .await?;
            if relays_to_use.is_empty() {
                tracing::warn!(
                    target: "whitenoise::session::groups",
                    "User {} has no relays configured, using account's default relays",
                    user.pubkey
                );
                relays_to_use = account.nip65_relays(wn).await?;
            }
            let relays_to_use_urls = Relay::urls(&relays_to_use);
            let some_event = wn
                .shared
                .relay_control
                .fetch_user_key_package(*pk, &relays_to_use_urls)
                .await?;
            let event = some_event.ok_or(WhitenoiseError::MdkCoreError(
                mdk_core::Error::KeyPackage("Does not exist".to_owned()),
            ))?;

            validate_fetched_member_key_package(&event, pk)?;

            key_package_events.push(event);
            users.push(user);
        }

        let relay_urls = self.ensure_relays(group_id)?;

        let update_result = self
            .session
            .mdk
            .add_members(group_id, &key_package_events)?;

        let evolution_event = update_result.evolution_event;
        let welcome_rumors = match update_result.welcome_rumors {
            None => {
                return Err(WhitenoiseError::MdkCoreError(mdk_core::Error::Group(
                    "Missing welcome message".to_owned(),
                )));
            }
            Some(wr) => wr,
        };

        if welcome_rumors.len() != users.len() {
            return Err(WhitenoiseError::Internal(
                "Welcome rumours are missing for some of the members".to_string(),
            ));
        }

        self.publish_and_merge_commit(evolution_event, group_id, &relay_urls)
            .await?;

        for (welcome_rumor, user) in welcome_rumors.iter().zip(users) {
            let key_package_event_id =
                welcome_rumor
                    .tags
                    .event_ids()
                    .next()
                    .ok_or(WhitenoiseError::Internal(
                        "No event ID found in welcome rumor".to_string(),
                    ))?;

            let member_pubkey = key_package_events
                .iter()
                .find(|event| event.id == *key_package_event_id)
                .map(|event| event.pubkey)
                .ok_or(WhitenoiseError::Internal(
                    "No public key found in key package event".to_string(),
                ))?;

            let one_month_future = Timestamp::now() + Duration::from_secs(30 * 24 * 60 * 60);

            let relays_to_use = wn
                .resolve_member_delivery_relays(
                    &user,
                    &account,
                    "whitenoise::session::groups::add_members",
                )
                .await?;

            let relay_urls = Relay::urls(&relays_to_use);

            wn.shared
                .relay_control
                .publish_welcome(
                    &member_pubkey,
                    welcome_rumor.clone(),
                    &[Tag::expiration(one_month_future)],
                    account.pubkey,
                    &relay_urls,
                    signer.clone(),
                )
                .await
                .map_err(WhitenoiseError::from)?;
        }

        Ok(())
    }

    /// Removes members from an existing MLS group.
    ///
    /// Creates an MLS remove-members proposal, publishes the evolution event,
    /// and merges the pending commit on success.
    pub async fn remove_members(&self, group_id: &GroupId, members: Vec<PublicKey>) -> Result<()> {
        self.ensure_admin(group_id)?;

        let relay_urls = self.ensure_relays(group_id)?;
        let update_result = self.session.mdk.remove_members(group_id, &members)?;

        self.publish_and_merge_commit(update_result.evolution_event, group_id, &relay_urls)
            .await
    }

    /// Updates group metadata and publishes the change to group relays.
    ///
    /// Creates an MLS group-data update proposal, publishes the evolution
    /// event, merges the pending commit, and refreshes subscriptions.
    // TODO(phase-16): Remove singleton bridge when background_refresh moves to session.
    pub async fn update_group_data(
        &self,
        group_id: &GroupId,
        group_data: NostrGroupDataUpdate,
    ) -> Result<()> {
        self.ensure_admin(group_id)?;

        let relay_urls = self.ensure_relays(group_id)?;
        let update_result = self.session.mdk.update_group_data(group_id, group_data)?;

        self.publish_and_merge_commit(update_result.evolution_event, group_id, &relay_urls)
            .await?;

        let wn = Self::wn()?;
        let account =
            Account::find_by_pubkey(&self.session.account_pubkey, &wn.shared.database).await?;
        wn.background_refresh_account_group_subscriptions(&account);
        Ok(())
    }

    /// Removes the caller from the group's admin list.
    ///
    /// This is a prerequisite for [`Self::leave`] — the MIP-03 protocol
    /// requires admins to relinquish admin status before sending a SelfRemove
    /// proposal.
    pub async fn self_demote(&self, group_id: &GroupId) -> Result<()> {
        self.ensure_admin(group_id)?;

        let relay_urls = self.ensure_relays(group_id)?;
        let update_result = self.session.mdk.self_demote(group_id)?;

        self.publish_and_merge_commit(update_result.evolution_event, group_id, &relay_urls)
            .await
    }

    /// Leaves a group by creating a SelfRemove proposal and publishing it.
    ///
    /// After the proposal is successfully published, the group is
    /// optimistically marked as departed locally. When another member
    /// auto-commits the proposal, the resulting commit converges local state.
    // TODO(phase-16): Remove singleton bridge when mark_as_left moves to session.
    #[allow(deprecated)] // wn.mark_as_left() deprecated in Phase 12
    pub async fn leave(&self, group_id: &GroupId) -> Result<()> {
        let wn = Self::wn()?;

        let account_group = AccountGroup::get(wn, &self.session.account_pubkey, group_id)
            .await?
            .ok_or(WhitenoiseError::GroupNotFound)?;

        if account_group.is_removed() {
            return Err(WhitenoiseError::AlreadyDepartedFromGroup);
        }

        let relay_urls = self.ensure_relays(group_id)?;
        let update_result = self.session.mdk.leave_group(group_id)?;

        wn.publish_event_with_retry(
            update_result.evolution_event,
            &self.session.account_pubkey,
            &relay_urls,
        )
        .await?;

        let account =
            Account::find_by_pubkey(&self.session.account_pubkey, &wn.shared.database).await?;

        if let Err(error) = wn.mark_as_left(&account, group_id).await {
            tracing::warn!(
                target: "whitenoise::session::groups",
                account_pubkey = %self.session.account_pubkey,
                group_id = %hex::encode(group_id.as_slice()),
                "SelfRemove published but failed to mark local departure: {error}",
            );
        }

        wn.background_refresh_account_group_subscriptions(&account);

        Ok(())
    }

    // ── Group creation ──────────────────────────────────────────────

    /// Creates a new MLS group with the specified members and settings.
    ///
    /// Welcome messages are delivered inline after the group is committed locally.
    /// If welcome delivery fails for a member, the failure is logged but does not
    /// prevent `Ok(group)` from being returned.
    // TODO(phase-16): Remove singleton bridge when relay_control moves to session.
    pub async fn create_group(
        &self,
        member_pubkeys: Vec<PublicKey>,
        config: NostrGroupConfigData,
        group_type: Option<GroupType>,
    ) -> Result<group_types::Group> {
        let wn = Self::wn()?;
        let signer = self
            .session
            .get_signer()
            .ok_or(WhitenoiseError::SignerUnavailable(
                self.session.account_pubkey,
            ))?;

        // Reject duplicate member pubkeys before doing any async work
        let unique: BTreeSet<&PublicKey> = member_pubkeys.iter().collect();
        if unique.len() != member_pubkeys.len() {
            return Err(WhitenoiseError::InvalidInput(
                "member_pubkeys contains duplicates".to_string(),
            ));
        }

        // Resolve members and fetch key packages concurrently
        let member_futures = member_pubkeys
            .iter()
            .map(|pk| self.resolve_member_key_package(pk));
        let resolved_members = try_join_all(member_futures).await?;
        let (members, key_package_events): (Vec<User>, Vec<Event>) =
            resolved_members.into_iter().unzip();

        tracing::debug!(
            target: "whitenoise::session::groups",
            "Successfully fetched the key packages of members"
        );

        let group_name = config.name.clone();

        let create_group_result = self.session.mdk.create_group(
            &self.session.account_pubkey,
            key_package_events.clone(),
            config,
        )?;

        let group = create_group_result.group;
        let welcome_data = Self::prepare_welcomes(
            create_group_result.welcome_rumors,
            members,
            &key_package_events,
        )?;

        self.finalize_group_records(&group, &member_pubkeys, group_type, &group_name)
            .await?;

        Self::publish_welcomes(welcome_data, signer, self.session.account_pubkey).await;

        let account =
            Account::find_by_pubkey(&self.session.account_pubkey, &wn.shared.database).await?;
        wn.background_refresh_account_group_subscriptions(&account);

        Ok(group)
    }

    /// Resolves a single member for group creation: finds or creates the user
    /// record, syncs relay lists for new users, fetches and validates the key
    /// package.
    // TODO(phase-16): Remove singleton bridge when relay_control moves to session.
    async fn resolve_member_key_package(&self, pk: &PublicKey) -> Result<(User, Event)> {
        let wn = Self::wn()?;
        let (user, created) = User::find_or_create_by_pubkey(pk, &wn.shared.database).await?;
        if created && let Err(e) = user.update_relay_lists(wn).await {
            tracing::warn!(
                target: "whitenoise::session::groups",
                "Failed to update relay lists for new user {}: {}",
                user.pubkey,
                e
            );
        }

        let some_event = user.key_package_event(wn).await?;
        let event = some_event.ok_or(WhitenoiseError::MdkCoreError(
            mdk_core::Error::KeyPackage("Does not exist".to_owned()),
        ))?;

        validate_fetched_member_key_package(&event, pk)?;

        Ok((user, event))
    }

    /// Validates and pairs welcome rumors with their target members and key
    /// package pubkeys.
    fn prepare_welcomes(
        welcome_rumors: Vec<UnsignedEvent>,
        members: Vec<User>,
        key_package_events: &[Event],
    ) -> Result<Vec<(UnsignedEvent, User, PublicKey)>> {
        if welcome_rumors.len() != members.len() {
            return Err(WhitenoiseError::Internal(
                "Welcome rumours are missing for some of the members".to_string(),
            ));
        }

        let kp_pubkey_by_event_id: HashMap<EventId, PublicKey> = key_package_events
            .iter()
            .map(|event| (event.id, event.pubkey))
            .collect();

        let mut members_by_pubkey: HashMap<PublicKey, User> = members
            .into_iter()
            .map(|member| (member.pubkey, member))
            .collect();

        welcome_rumors
            .into_iter()
            .map(|rumor| {
                let kp_event_id = rumor.tags.event_ids().next().ok_or_else(|| {
                    WhitenoiseError::Internal("No event ID found in welcome rumor".to_string())
                })?;
                let member_pubkey = kp_pubkey_by_event_id.get(kp_event_id).copied().ok_or(
                    WhitenoiseError::Internal(
                        "No public key found in key package event".to_string(),
                    ),
                )?;
                let member =
                    members_by_pubkey
                        .remove(&member_pubkey)
                        .ok_or(WhitenoiseError::Internal(format!(
                            "No member record found for welcome target {}",
                            member_pubkey
                        )))?;
                Ok((rumor, member, member_pubkey))
            })
            .collect()
    }

    /// Creates local database records for a newly created group:
    /// GroupInformation and AccountGroup (auto-accepted for the creator).
    // TODO(phase-16): Remove singleton bridge when push ops fully on session.
    async fn finalize_group_records(
        &self,
        group: &group_types::Group,
        member_pubkeys: &[PublicKey],
        group_type: Option<GroupType>,
        group_name: &str,
    ) -> Result<()> {
        let group_type = group_type
            .unwrap_or_else(|| GroupInformation::infer_group_type_from_group_name(group_name));

        // For DM groups, the peer is the single member we're creating the group with
        let dm_peer = if group_type == GroupType::DirectMessage {
            member_pubkeys.first()
        } else {
            None
        };

        let (_group_info, _was_created) = GroupInformation::find_or_create_by_mls_group_id(
            &group.mls_group_id,
            Some(group_type),
            &self.session.database,
        )
        .await?;

        let (account_group, _) = AccountGroup::find_or_create(
            &self.session.account_pubkey,
            &group.mls_group_id,
            dm_peer,
            &self.session.database,
        )
        .await?;
        account_group
            .update_user_confirmation(true, &self.session.database)
            .await?;

        // Best-effort: share the creator's push token into the new group.
        if let Err(error) = self
            .session
            .push()
            .share_local_token_to_group(&group.mls_group_id)
            .await
        {
            tracing::warn!(
                target: "whitenoise::session::groups",
                account = %self.session.account_pubkey.to_hex(),
                group = %hex::encode(group.mls_group_id.as_slice()),
                error = %error,
                "Failed to share local push token after group creation"
            );
        }

        Ok(())
    }

    /// Delivers welcome messages to all group members, attempting every member even if
    /// individual publishes fail. Errors are logged as warnings rather than propagated —
    /// missed welcomes are non-fatal; the relay layer retries with exponential backoff.
    ///
    /// The caller is responsible for deciding whether to `tokio::spawn` this for
    /// fire-and-forget behaviour. The view itself does not spawn.
    // TODO(phase-16): Replace singleton bridge with session-owned relay_control.
    async fn publish_welcomes(
        welcome_data: Vec<(UnsignedEvent, User, PublicKey)>,
        signer: Arc<dyn NostrSigner>,
        creator_pubkey: PublicKey,
    ) {
        let whitenoise = match Whitenoise::get_instance() {
            Ok(wn) => wn,
            Err(error) => {
                tracing::error!(
                    target: "whitenoise::session::groups",
                    "Failed to get Whitenoise instance for welcome publishing: {}",
                    error
                );
                return;
            }
        };

        let creator_account =
            match Account::find_by_pubkey(&creator_pubkey, &whitenoise.shared.database).await {
                Ok(account) => account,
                Err(error) => {
                    tracing::error!(
                        target: "whitenoise::session::groups",
                        "Failed to find creator account for welcome publishing: {}",
                        error
                    );
                    return;
                }
            };

        let futures = welcome_data
            .into_iter()
            .map(|(rumor, member, member_pubkey)| {
                let signer = signer.clone();
                let creator = &creator_account;
                async move {
                    let relays_to_use = whitenoise
                        .resolve_member_delivery_relays(
                            &member,
                            creator,
                            "whitenoise::session::groups::create_group",
                        )
                        .await?;

                    let one_month_future =
                        Timestamp::now() + Duration::from_secs(30 * 24 * 60 * 60);

                    whitenoise
                        .shared
                        .relay_control
                        .publish_welcome(
                            &member_pubkey,
                            rumor,
                            &[Tag::expiration(one_month_future)],
                            creator.pubkey,
                            &Relay::urls(&relays_to_use),
                            signer,
                        )
                        .await
                        .map_err(WhitenoiseError::from)?;

                    Ok::<(), WhitenoiseError>(())
                }
            });

        let results = join_all(futures).await;
        for result in results {
            if let Err(error) = result {
                tracing::warn!(
                    target: "whitenoise::session::groups",
                    "Welcome publish failed: {}",
                    error
                );
            }
        }
    }

    /// Return a view for group media operations scoped to this session.
    pub fn media(&self) -> MediaOps<'_> {
        MediaOps::new(self.session)
    }
}

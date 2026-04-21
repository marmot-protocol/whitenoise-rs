//! Group read and mutation operations scoped to an [`AccountSession`].

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::time::Duration;

use mdk_core::prelude::*;
use nostr_sdk::prelude::*;
use nostr_sdk::{PublicKey, RelayUrl};

use super::AccountSession;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::accounts_groups::AccountGroup;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::group_information::GroupInformation;
use crate::whitenoise::groups::{GroupWithInfoAndMembership, GroupWithMembership};
use crate::whitenoise::key_packages::{
    REQUIRED_MLS_CIPHERSUITE_TAG, validate_marmot_key_package_tags,
};
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
        let account = Account::find_by_pubkey(&self.session.account_pubkey, &wn.database).await?;

        let mut key_package_events: Vec<Event> = Vec::new();
        let mut users = Vec::new();

        for pk in member_pubkeys.iter() {
            let (user, newly_created) = User::find_or_create_by_pubkey(pk, &wn.database).await?;

            if newly_created && let Err(e) = user.update_relay_lists(wn).await {
                tracing::warn!(
                    target: "whitenoise::session::groups",
                    "Failed to update relay lists for new user {}: {}",
                    user.pubkey,
                    e
                );
            }

            let mut relays_to_use = user.relays(RelayType::KeyPackage, &wn.database).await?;
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
                .relay_control
                .fetch_user_key_package(*pk, &relays_to_use_urls)
                .await?;
            let event = some_event.ok_or(WhitenoiseError::MdkCoreError(
                mdk_core::Error::KeyPackage("Does not exist".to_owned()),
            ))?;

            Self::validate_fetched_member_key_package(&event, pk)?;

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

            wn.relay_control
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
        let account = Account::find_by_pubkey(&self.session.account_pubkey, &wn.database).await?;
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

        let account = Account::find_by_pubkey(&self.session.account_pubkey, &wn.database).await?;

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

    // ── Static helpers ────────────────────────────────────────────────

    fn validate_fetched_member_key_package(event: &Event, pk: &PublicKey) -> Result<()> {
        if event.pubkey != *pk {
            return Err(WhitenoiseError::InvalidInput(format!(
                "Fetched key package event {} signed by {} instead of expected {}",
                event.id, event.pubkey, pk
            )));
        }

        validate_marmot_key_package_tags(event, REQUIRED_MLS_CIPHERSUITE_TAG).map_err(|e| {
            WhitenoiseError::InvalidInput(format!(
                "Incompatible key package event {} for member {}: {}",
                event.id, pk, e
            ))
        })?;

        Ok(())
    }
}

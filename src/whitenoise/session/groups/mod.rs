//! Group read and mutation operations scoped to an [`AccountSession`].

mod media;

pub use self::media::MediaOps;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use ::rand::RngCore;
use cgka_traits::TransportEndpoint;
use cgka_traits::agent_text_stream::AgentTextStreamQuicPolicyV1;
use cgka_traits::app_components::{
    AppComponentData, GROUP_ADMIN_POLICY_COMPONENT_ID, GROUP_MESSAGE_RETENTION_COMPONENT_ID,
    GROUP_PROFILE_COMPONENT_ID, NOSTR_ROUTING_COMPONENT_ID, NostrRoutingV1,
    encode_component_vectors, encode_nostr_routing_v1, encode_quic_varint,
};
use cgka_traits::engine::CreateGroupRequest;
use cgka_traits::engine::KeyPackage;
use cgka_traits::types::{GroupId as MarmotGroupId, MemberId};
use chrono::Utc;
use futures::future::try_join_all;
use nostr_sdk::prelude::*;

use super::AccountSession;
use crate::marmot::key_packages::key_package_from_v2_event;
use crate::marmot::publish::{
    MarmotMessagePublisher, MarmotPendingResolution, MarmotPublishedEffects, publish_effects,
};
use crate::marmot::session::MarmotSessionEffects;
use crate::marmot::transport::{
    MarmotGroupPublishRoute, MarmotPublishRoutes, MarmotRelayControlPublisher,
};
use crate::marmot::{
    GroupConfig, GroupDataUpdate, GroupId, MarmotCreatedGroupProjection, group_types,
};
use crate::relay_control::ephemeral::KeyPackageLookup;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::accounts_groups::AccountGroup;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::group_information::{GroupInformation, GroupType};
use crate::whitenoise::groups::{GroupWithInfoAndMembership, GroupWithMembership};
use crate::whitenoise::relays::Relay;
use crate::whitenoise::users::User;

/// View over [`AccountSession`] for group operations.
///
/// Obtain via [`AccountSession::groups`].
pub struct GroupOps<'a> {
    session: &'a AccountSession,
}

#[cfg_attr(not(test), expect(dead_code, reason = "phase 7"))]
pub(crate) struct MarmotResolvedMemberKeyPackage {
    pub(crate) user: User,
    pub(crate) event: Event,
    pub(crate) key_package: KeyPackage,
}

pub(crate) struct MarmotGroupCreationPlan {
    pub(crate) request: CreateGroupRequest,
    pub(crate) routing: NostrRoutingV1,
    pub(crate) member_pubkeys: Vec<PublicKey>,
    pub(crate) resolved_members: Vec<MarmotResolvedMemberKeyPackage>,
}

impl<'a> GroupOps<'a> {
    pub(super) fn new(session: &'a AccountSession) -> Self {
        Self { session }
    }

    // ── Read operations ───────────────────────────────────────────────

    /// Return all groups for this account.
    ///
    /// When `active_filter` is `true`, only groups in `Active` state are
    /// returned. When `false`, all groups (including inactive) are included.
    pub fn all(&self, active_filter: bool) -> Result<Vec<group_types::Group>> {
        let mut groups = self.marmot_groups()?;
        groups.retain(|group| !active_filter || group.state == group_types::GroupState::Active);

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
    /// All projected groups are read so removed groups can stay paired with
    /// their `AccountGroup` records.
    pub async fn visible(&self) -> Result<Vec<GroupWithMembership>> {
        let all_groups = self.all(false)?;

        let visible_account_groups = AccountGroup::find_visible_for_account(
            &self.session.account_pubkey,
            &self.session.account_db.inner.pool,
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
    /// info individually. Missing `group_information` rows are repaired from
    /// confirmed Marmot group projections when possible; groups with no
    /// available information are excluded.
    pub async fn visible_with_info(&self) -> Result<Vec<GroupWithInfoAndMembership>> {
        let visible = self.visible().await?;

        if visible.is_empty() {
            return Ok(Vec::new());
        }

        let group_ids: Vec<_> = visible
            .iter()
            .map(|gwm| gwm.group.mls_group_id.clone())
            .collect();
        let info_list = GroupInformation::get_by_mls_group_ids_with_marmot_storage(
            &group_ids,
            &self.session.marmot_storage,
            &self.session.shared.database,
        )
        .await?;
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
        if let Some(group) = self.marmot_group(group_id)? {
            return Ok(group);
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    /// Return the members of a group.
    pub fn members(&self, group_id: &GroupId) -> Result<Vec<PublicKey>> {
        if let Some(projection) = self.marmot_group_projection(group_id)? {
            return Ok(projection.member_pubkeys.into_iter().collect());
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    /// Return the relay URLs for a group.
    pub fn relays(&self, group_id: &GroupId) -> Result<BTreeSet<RelayUrl>> {
        if let Some(projection) = self.marmot_group_projection(group_id)? {
            return Self::marmot_projection_relays(projection);
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    /// Return the admin public keys for a group.
    pub fn admins(&self, group_id: &GroupId) -> Result<Vec<PublicKey>> {
        Ok(self.get(group_id)?.admin_pubkeys.into_iter().collect())
    }

    fn marmot_groups(&self) -> Result<Vec<group_types::Group>> {
        self.session
            .marmot_storage
            .list_group_projections()?
            .into_iter()
            .map(Self::marmot_projection_to_group)
            .collect()
    }

    fn marmot_group(&self, group_id: &GroupId) -> Result<Option<group_types::Group>> {
        self.marmot_group_projection(group_id)?
            .map(Self::marmot_projection_to_group)
            .transpose()
    }

    fn marmot_group_projection(
        &self,
        group_id: &GroupId,
    ) -> Result<Option<MarmotCreatedGroupProjection>> {
        self.session
            .marmot_storage
            .find_group_projection(&cgka_traits::types::GroupId::new(
                group_id.as_slice().to_vec(),
            ))
            .map_err(WhitenoiseError::from)
    }

    fn marmot_projection_to_group(
        projection: MarmotCreatedGroupProjection,
    ) -> Result<group_types::Group> {
        Ok(group_types::Group {
            mls_group_id: GroupId::from_slice(projection.group_id.as_slice()),
            nostr_group_id: projection.routing.nostr_group_id,
            name: projection.name,
            description: projection.description,
            image_hash: None,
            image_key: None,
            image_nonce: None,
            admin_pubkeys: projection.admin_pubkeys,
            last_message_id: None,
            last_message_at: None,
            last_message_processed_at: None,
            epoch: projection.epoch,
            state: group_types::GroupState::Active,
            // Public read shape retained for callers that still display the
            // upgrade state; confirmed Darkmatter groups project as completed.
            self_update_state: group_types::SelfUpdateState::CompletedAt(Timestamp::from_secs(
                projection.self_update_completed_at_secs,
            )),
            disappearing_message_secs: projection.disappearing_message_secs,
        })
    }

    fn marmot_projection_relays(
        projection: MarmotCreatedGroupProjection,
    ) -> Result<BTreeSet<RelayUrl>> {
        projection
            .routing
            .relays
            .into_iter()
            .map(|relay| RelayUrl::parse(&relay).map_err(WhitenoiseError::from))
            .collect()
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

    // ── Mutation operations ───────────────────────────────────────────

    /// Adds new members to an existing MLS group.
    ///
    /// Performs the complete workflow: fetch v2 key packages, add members via
    /// Darkmatter, publish evolution effects, and send welcome messages.
    pub async fn add_members(
        &self,
        group_id: &GroupId,
        member_pubkeys: Vec<PublicKey>,
    ) -> Result<()> {
        if self.marmot_group_projection(group_id)?.is_some() {
            return self.add_marmot_members(group_id, member_pubkeys).await;
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    /// Removes members from an existing MLS group.
    ///
    /// Creates an MLS remove-members proposal, publishes the evolution event,
    /// and merges the pending commit on success.
    pub async fn remove_members(&self, group_id: &GroupId, members: Vec<PublicKey>) -> Result<()> {
        if self.marmot_group_projection(group_id)?.is_some() {
            return self.remove_marmot_members(group_id, members).await;
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    /// Updates group metadata and publishes the change to group relays.
    ///
    /// Creates an MLS group-data update proposal, publishes the evolution
    /// event, merges the pending commit, and refreshes subscriptions.
    pub async fn update_group_data(
        &self,
        group_id: &GroupId,
        group_data: GroupDataUpdate,
    ) -> Result<()> {
        if let Some(projection) = self.marmot_group_projection(group_id)? {
            return self
                .update_marmot_group_data(group_id, group_data, projection)
                .await;
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    /// Removes the caller from the group's admin list.
    ///
    /// This is a prerequisite for [`Self::leave`] — the MIP-03 protocol
    /// requires admins to relinquish admin status before sending a SelfRemove
    /// proposal.
    pub async fn self_demote(&self, group_id: &GroupId) -> Result<()> {
        if let Some(projection) = self.marmot_group_projection(group_id)? {
            return self.self_demote_marmot(group_id, projection).await;
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    /// Leaves a group by creating a SelfRemove proposal and publishing it.
    ///
    /// After the proposal is successfully published, the group is
    /// optimistically marked as departed locally. When another member
    /// auto-commits the proposal, the resulting commit converges local state.
    pub async fn leave(&self, group_id: &GroupId) -> Result<()> {
        if self.marmot_group_projection(group_id)?.is_some() {
            return self.leave_marmot_group(group_id).await;
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    // ── Group creation ──────────────────────────────────────────────

    /// Creates a new Marmot group with the specified members and settings.
    pub async fn create_group(
        &self,
        member_pubkeys: Vec<PublicKey>,
        config: GroupConfig,
        group_type: Option<GroupType>,
    ) -> Result<group_types::Group> {
        self.create_marmot_group(member_pubkeys, config, group_type)
            .await
    }

    pub(crate) async fn create_marmot_group(
        &self,
        member_pubkeys: Vec<PublicKey>,
        config: GroupConfig,
        group_type: Option<GroupType>,
    ) -> Result<group_types::Group> {
        let plan = self
            .prepare_marmot_group_creation(member_pubkeys, config)
            .await?;
        self.create_marmot_group_from_plan(plan, group_type).await
    }

    async fn create_marmot_group_from_plan(
        &self,
        plan: MarmotGroupCreationPlan,
        group_type: Option<GroupType>,
    ) -> Result<group_types::Group> {
        let account =
            Account::find_by_pubkey(&self.session.account_pubkey, &self.session.shared.database)
                .await?;
        let marmot_session = self.marmot_session()?;
        let created = {
            let mut session = marmot_session.lock().await;
            session.create_group(plan.request.clone()).await?
        };
        let routes = self
            .marmot_publish_routes_for_group_creation(&plan, &created.group_id, &account)
            .await?;
        let publisher = MarmotRelayControlPublisher::new(&self.session.ephemeral, routes);

        self.finish_marmot_group_creation(marmot_session, &plan, group_type, &publisher, created)
            .await
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "phase 7"))]
    pub(crate) async fn create_marmot_group_with_publisher<P>(
        &self,
        member_pubkeys: Vec<PublicKey>,
        config: GroupConfig,
        group_type: Option<GroupType>,
        publisher: &P,
    ) -> Result<group_types::Group>
    where
        P: MarmotMessagePublisher + Sync + ?Sized,
    {
        let plan = self
            .prepare_marmot_group_creation(member_pubkeys, config)
            .await?;
        let marmot_session = self.marmot_session()?;
        let created = {
            let mut session = marmot_session.lock().await;
            session.create_group(plan.request.clone()).await?
        };

        self.finish_marmot_group_creation(marmot_session, &plan, group_type, publisher, created)
            .await
    }

    async fn add_marmot_members(
        &self,
        group_id: &GroupId,
        member_pubkeys: Vec<PublicKey>,
    ) -> Result<()> {
        self.ensure_admin(group_id)?;
        let account =
            Account::find_by_pubkey(&self.session.account_pubkey, &self.session.shared.database)
                .await?;
        let resolved_members = self.prepare_marmot_member_addition(member_pubkeys).await?;
        let marmot_group_id = MarmotGroupId::new(group_id.as_slice().to_vec());
        let routes = self
            .marmot_publish_routes_for_member_addition(
                &marmot_group_id,
                &resolved_members,
                &account,
            )
            .await?;
        let publisher = MarmotRelayControlPublisher::new(&self.session.ephemeral, routes);

        self.add_marmot_members_with_resolved(&marmot_group_id, resolved_members, &publisher)
            .await
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "phase 11"))]
    pub(crate) async fn add_marmot_members_with_publisher<P>(
        &self,
        group_id: &GroupId,
        member_pubkeys: Vec<PublicKey>,
        publisher: &P,
    ) -> Result<()>
    where
        P: MarmotMessagePublisher + Sync + ?Sized,
    {
        self.ensure_admin(group_id)?;
        let resolved_members = self.prepare_marmot_member_addition(member_pubkeys).await?;
        let marmot_group_id = MarmotGroupId::new(group_id.as_slice().to_vec());

        self.add_marmot_members_with_resolved(&marmot_group_id, resolved_members, publisher)
            .await
    }

    async fn add_marmot_members_with_resolved<P>(
        &self,
        marmot_group_id: &MarmotGroupId,
        resolved_members: Vec<MarmotResolvedMemberKeyPackage>,
        publisher: &P,
    ) -> Result<()>
    where
        P: MarmotMessagePublisher + Sync + ?Sized,
    {
        let key_packages = resolved_members
            .into_iter()
            .map(|resolved| resolved.key_package)
            .collect();
        let marmot_session = self.marmot_session()?;
        let effects = {
            let mut session = marmot_session.lock().await;
            session
                .invite_members(marmot_group_id.clone(), key_packages)
                .await?
        };

        self.finish_marmot_group_mutation(marmot_session, marmot_group_id, publisher, effects)
            .await
    }

    async fn remove_marmot_members(
        &self,
        group_id: &GroupId,
        members: Vec<PublicKey>,
    ) -> Result<()> {
        self.ensure_admin(group_id)?;
        let marmot_group_id = MarmotGroupId::new(group_id.as_slice().to_vec());
        let routes = self
            .marmot_publish_routes_for_existing_group(&marmot_group_id)
            .await?;
        let publisher = MarmotRelayControlPublisher::new(&self.session.ephemeral, routes);

        self.remove_marmot_members_with_publisher(group_id, members, &publisher)
            .await
    }

    pub(crate) async fn remove_marmot_members_with_publisher<P>(
        &self,
        group_id: &GroupId,
        members: Vec<PublicKey>,
        publisher: &P,
    ) -> Result<()>
    where
        P: MarmotMessagePublisher + Sync + ?Sized,
    {
        self.ensure_admin(group_id)?;
        let marmot_group_id = MarmotGroupId::new(group_id.as_slice().to_vec());
        let member_ids = members
            .into_iter()
            .map(|member| MemberId::new(member.as_bytes().to_vec()))
            .collect();
        let marmot_session = self.marmot_session()?;
        let effects = {
            let mut session = marmot_session.lock().await;
            session
                .remove_members(marmot_group_id.clone(), member_ids)
                .await?
        };

        self.finish_marmot_group_mutation(marmot_session, &marmot_group_id, publisher, effects)
            .await
    }

    async fn leave_marmot_group(&self, group_id: &GroupId) -> Result<()> {
        let marmot_group_id = MarmotGroupId::new(group_id.as_slice().to_vec());
        let routes = self
            .marmot_publish_routes_for_existing_group(&marmot_group_id)
            .await?;
        let publisher = MarmotRelayControlPublisher::new(&self.session.ephemeral, routes);

        self.leave_marmot_group_with_publisher(group_id, &publisher)
            .await
    }

    pub(crate) async fn leave_marmot_group_with_publisher<P>(
        &self,
        group_id: &GroupId,
        publisher: &P,
    ) -> Result<()>
    where
        P: MarmotMessagePublisher + Sync + ?Sized,
    {
        self.ensure_group_can_be_left(group_id).await?;
        let marmot_group_id = MarmotGroupId::new(group_id.as_slice().to_vec());
        let marmot_session = self.marmot_session()?;
        let effects = {
            let mut session = marmot_session.lock().await;
            session.leave_group(marmot_group_id.clone()).await?
        };

        let published = publish_effects(marmot_session, publisher, effects).await?;
        Self::ensure_marmot_leave_published(&published)?;

        self.mark_local_departure_after_self_remove(group_id).await
    }

    async fn update_marmot_group_data(
        &self,
        group_id: &GroupId,
        group_data: GroupDataUpdate,
        projection: MarmotCreatedGroupProjection,
    ) -> Result<()> {
        self.ensure_admin(group_id)?;
        let updates = Self::marmot_group_data_update_components(group_data, &projection)?;
        let marmot_group_id = MarmotGroupId::new(group_id.as_slice().to_vec());
        let routes = self
            .marmot_publish_routes_for_existing_group(&marmot_group_id)
            .await?;
        let publisher = MarmotRelayControlPublisher::new(&self.session.ephemeral, routes);

        self.update_marmot_group_data_with_components(&marmot_group_id, updates, &publisher)
            .await
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "phase 11"))]
    pub(crate) async fn update_marmot_group_data_with_publisher<P>(
        &self,
        group_id: &GroupId,
        group_data: GroupDataUpdate,
        publisher: &P,
    ) -> Result<()>
    where
        P: MarmotMessagePublisher + Sync + ?Sized,
    {
        self.ensure_admin(group_id)?;
        let projection = self
            .marmot_group_projection(group_id)?
            .ok_or(WhitenoiseError::GroupNotFound)?;
        let updates = Self::marmot_group_data_update_components(group_data, &projection)?;
        let marmot_group_id = MarmotGroupId::new(group_id.as_slice().to_vec());

        self.update_marmot_group_data_with_components(&marmot_group_id, updates, publisher)
            .await
    }

    async fn update_marmot_group_data_with_components<P>(
        &self,
        marmot_group_id: &MarmotGroupId,
        updates: Vec<AppComponentData>,
        publisher: &P,
    ) -> Result<()>
    where
        P: MarmotMessagePublisher + Sync + ?Sized,
    {
        let marmot_session = self.marmot_session()?;
        let effects = {
            let mut session = marmot_session.lock().await;
            session
                .update_app_components((*marmot_group_id).clone(), updates)
                .await?
        };

        self.finish_marmot_group_mutation(marmot_session, marmot_group_id, publisher, effects)
            .await
    }

    async fn self_demote_marmot(
        &self,
        group_id: &GroupId,
        projection: MarmotCreatedGroupProjection,
    ) -> Result<()> {
        self.ensure_admin(group_id)?;
        let marmot_group_id = MarmotGroupId::new(group_id.as_slice().to_vec());
        let routes = self
            .marmot_publish_routes_for_existing_group(&marmot_group_id)
            .await?;
        let publisher = MarmotRelayControlPublisher::new(&self.session.ephemeral, routes);

        self.self_demote_marmot_with_projection(&marmot_group_id, projection, &publisher)
            .await
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "phase 11"))]
    pub(crate) async fn self_demote_marmot_with_publisher<P>(
        &self,
        group_id: &GroupId,
        publisher: &P,
    ) -> Result<()>
    where
        P: MarmotMessagePublisher + Sync + ?Sized,
    {
        self.ensure_admin(group_id)?;
        let projection = self
            .marmot_group_projection(group_id)?
            .ok_or(WhitenoiseError::GroupNotFound)?;
        let marmot_group_id = MarmotGroupId::new(group_id.as_slice().to_vec());

        self.self_demote_marmot_with_projection(&marmot_group_id, projection, publisher)
            .await
    }

    async fn self_demote_marmot_with_projection<P>(
        &self,
        marmot_group_id: &MarmotGroupId,
        projection: MarmotCreatedGroupProjection,
        publisher: &P,
    ) -> Result<()>
    where
        P: MarmotMessagePublisher + Sync + ?Sized,
    {
        let admins = projection
            .admin_pubkeys
            .iter()
            .copied()
            .filter(|admin| *admin != self.session.account_pubkey)
            .collect::<Vec<_>>();
        let update = AppComponentData {
            component_id: GROUP_ADMIN_POLICY_COMPONENT_ID,
            data: Self::encode_marmot_admin_policy(&admins, &projection.member_pubkeys)?,
        };

        self.update_marmot_group_data_with_components(marmot_group_id, vec![update], publisher)
            .await
    }

    fn marmot_group_data_update_components(
        group_data: GroupDataUpdate,
        projection: &MarmotCreatedGroupProjection,
    ) -> Result<Vec<AppComponentData>> {
        if group_data.image_hash.is_some()
            || group_data.image_key.is_some()
            || group_data.image_nonce.is_some()
            || group_data.image_upload_key.is_some()
        {
            return Err(WhitenoiseError::InvalidInput(
                "Darkmatter group image updates require media type support".to_string(),
            ));
        }

        let mut updates = Vec::new();

        if group_data.name.is_some() || group_data.description.is_some() {
            let name = group_data.name.unwrap_or_else(|| projection.name.clone());
            let description = group_data
                .description
                .unwrap_or_else(|| projection.description.clone());
            updates.push(AppComponentData {
                component_id: GROUP_PROFILE_COMPONENT_ID,
                data: encode_component_vectors(&[name.as_bytes(), description.as_bytes()]),
            });
        }

        if group_data.relays.is_some() || group_data.nostr_group_id.is_some() {
            let nostr_group_id = group_data
                .nostr_group_id
                .unwrap_or(projection.routing.nostr_group_id);
            let relays = group_data
                .relays
                .map(|relays| relays.into_iter().map(|relay| relay.to_string()).collect())
                .unwrap_or_else(|| projection.routing.relays.clone());
            let routing = NostrRoutingV1::new(nostr_group_id, relays).map_err(|error| {
                WhitenoiseError::InvalidInput(format!(
                    "invalid marmot.transport.nostr.routing.v1: {error}"
                ))
            })?;
            updates.push(AppComponentData {
                component_id: NOSTR_ROUTING_COMPONENT_ID,
                data: encode_nostr_routing_v1(&routing).map_err(|error| {
                    WhitenoiseError::InvalidInput(format!(
                        "invalid marmot.transport.nostr.routing.v1: {error}"
                    ))
                })?,
            });
        }

        if let Some(admins) = group_data.admins {
            updates.push(AppComponentData {
                component_id: GROUP_ADMIN_POLICY_COMPONENT_ID,
                data: Self::encode_marmot_admin_policy(&admins, &projection.member_pubkeys)?,
            });
        }

        if let Some(disappearing_message_secs) = group_data.disappearing_message_secs {
            updates.push(AppComponentData {
                component_id: GROUP_MESSAGE_RETENTION_COMPONENT_ID,
                data: disappearing_message_secs
                    .unwrap_or(0)
                    .to_be_bytes()
                    .to_vec(),
            });
        }

        if updates.is_empty() {
            return Err(WhitenoiseError::InvalidInput(
                "group_data contains no fields to update".to_string(),
            ));
        }

        Ok(updates)
    }

    fn encode_marmot_admin_policy(
        admins: &[PublicKey],
        members: &BTreeSet<PublicKey>,
    ) -> Result<Vec<u8>> {
        if admins.is_empty() {
            return Err(WhitenoiseError::InvalidInput(
                "admin policy must contain at least one admin".to_string(),
            ));
        }

        let mut unique_admins = BTreeSet::new();
        for admin in admins {
            if !unique_admins.insert(*admin) {
                return Err(WhitenoiseError::InvalidInput(
                    "admin list contains duplicates".to_string(),
                ));
            }
            if !members.contains(admin) {
                return Err(WhitenoiseError::InvalidInput(
                    "admin must be a group member".to_string(),
                ));
            }
        }

        let mut admin_bytes = Vec::with_capacity(unique_admins.len() * 32);
        encode_quic_varint((unique_admins.len() * 32) as u64, &mut admin_bytes);
        for admin in unique_admins {
            admin_bytes.extend_from_slice(admin.as_bytes());
        }

        Ok(admin_bytes)
    }

    async fn finish_marmot_group_creation<P>(
        &self,
        marmot_session: Arc<tokio::sync::Mutex<crate::marmot::session::MarmotSession>>,
        plan: &MarmotGroupCreationPlan,
        group_type: Option<GroupType>,
        publisher: &P,
        created: crate::marmot::session::CreateGroupEffects,
    ) -> Result<group_types::Group>
    where
        P: MarmotMessagePublisher + Sync + ?Sized,
    {
        let published = publish_effects(marmot_session.clone(), publisher, created.effects).await?;
        Self::ensure_marmot_group_creation_published(&published)?;

        let projection = {
            let session = marmot_session.lock().await;
            session.created_group_projection(&created.group_id)?
        };
        let group_id = self
            .project_created_marmot_group(&projection, &plan.member_pubkeys, group_type)
            .await?;

        let account =
            Account::find_by_pubkey(&self.session.account_pubkey, &self.session.shared.database)
                .await?;
        self.session
            .whitenoise()?
            .background_refresh_account_group_subscriptions(&account);

        self.get(&group_id)
    }

    fn marmot_session(
        &self,
    ) -> Result<Arc<tokio::sync::Mutex<crate::marmot::session::MarmotSession>>> {
        self.session
            .marmot
            .clone()
            .ok_or(WhitenoiseError::MarmotSessionUnavailable(
                self.session.account_pubkey,
            ))
    }

    async fn ensure_group_can_be_left(&self, group_id: &GroupId) -> Result<()> {
        let account_group = AccountGroup::find_by_account_and_group(
            &self.session.account_pubkey,
            group_id,
            &self.session.account_db.inner.pool,
        )
        .await?
        .ok_or(WhitenoiseError::GroupNotFound)?;

        if account_group.is_removed() {
            return Err(WhitenoiseError::AlreadyDepartedFromGroup);
        }

        Ok(())
    }

    fn ensure_marmot_group_creation_published(published: &MarmotPublishedEffects) -> Result<()> {
        if let Some(failure) = published.failures.first() {
            return Err(WhitenoiseError::MarmotPublishFailed(failure.reason.clone()));
        }

        if published
            .pending
            .iter()
            .any(|resolution| matches!(resolution, MarmotPendingResolution::RolledBack { .. }))
        {
            return Err(WhitenoiseError::MarmotPublishFailed(
                "pending group creation was rolled back".to_string(),
            ));
        }

        if !published
            .pending
            .iter()
            .any(|resolution| matches!(resolution, MarmotPendingResolution::Confirmed { .. }))
        {
            return Err(WhitenoiseError::Internal(
                "Marmot group creation produced no confirmed pending state".to_string(),
            ));
        }

        Ok(())
    }

    fn ensure_marmot_group_mutation_published(published: &MarmotPublishedEffects) -> Result<()> {
        if !published.queued.is_empty() {
            return Err(WhitenoiseError::MarmotPublishFailed(
                "Marmot group mutation was queued; queued mutation projection is not implemented"
                    .to_string(),
            ));
        }

        if published
            .pending
            .iter()
            .any(|resolution| matches!(resolution, MarmotPendingResolution::RolledBack { .. }))
        {
            let reason = published
                .failures
                .first()
                .map(|failure| failure.reason.clone())
                .unwrap_or_else(|| "pending group mutation was rolled back".to_string());
            return Err(WhitenoiseError::MarmotPublishFailed(reason));
        }

        if !published
            .pending
            .iter()
            .any(|resolution| matches!(resolution, MarmotPendingResolution::Confirmed { .. }))
        {
            return Err(WhitenoiseError::Internal(
                "Marmot group mutation produced no confirmed pending state".to_string(),
            ));
        }

        Ok(())
    }

    fn ensure_marmot_leave_published(published: &MarmotPublishedEffects) -> Result<()> {
        if !published.queued.is_empty() {
            return Err(WhitenoiseError::MarmotPublishFailed(
                "Marmot leave was queued; queued leave projection is not implemented".to_string(),
            ));
        }

        if let Some(failure) = published.failures.first() {
            return Err(WhitenoiseError::MarmotPublishFailed(failure.reason.clone()));
        }

        if published.reports.is_empty() {
            return Err(WhitenoiseError::Internal(
                "Marmot leave produced no SelfRemove proposal to publish".to_string(),
            ));
        }

        Ok(())
    }

    async fn finish_marmot_group_mutation<P>(
        &self,
        marmot_session: Arc<tokio::sync::Mutex<crate::marmot::session::MarmotSession>>,
        marmot_group_id: &MarmotGroupId,
        publisher: &P,
        effects: MarmotSessionEffects,
    ) -> Result<()>
    where
        P: MarmotMessagePublisher + Sync + ?Sized,
    {
        let published = publish_effects(marmot_session.clone(), publisher, effects).await?;
        Self::ensure_marmot_group_mutation_published(&published)?;

        for failure in &published.failures {
            tracing::warn!(
                target: "whitenoise::session::groups",
                group_id = %hex::encode(marmot_group_id.as_slice()),
                message_id = %hex::encode(failure.message_id.as_slice()),
                reason = %failure.reason,
                "Marmot group mutation committed, but an auxiliary publish failed"
            );
        }

        self.refresh_marmot_group_projection(marmot_session, marmot_group_id)
            .await?;
        let account =
            Account::find_by_pubkey(&self.session.account_pubkey, &self.session.shared.database)
                .await?;
        self.session
            .whitenoise()?
            .background_refresh_account_group_subscriptions(&account);

        Ok(())
    }

    async fn mark_local_departure_after_self_remove(&self, group_id: &GroupId) -> Result<()> {
        let account =
            Account::find_by_pubkey(&self.session.account_pubkey, &self.session.shared.database)
                .await?;

        if let Err(error) = self
            .session
            .membership()
            .for_group(group_id)
            .mark_as_left()
            .await
        {
            tracing::warn!(
                target: "whitenoise::session::groups",
                account_pubkey = %self.session.account_pubkey,
                group_id = %hex::encode(group_id.as_slice()),
                "SelfRemove published but failed to mark local departure: {error}",
            );
        }

        // background_refresh_account_group_subscriptions still genuinely needs
        // Whitenoise (account_manager + tokio::spawn). Inline upgrade keeps the
        // back-ref scoped to a single call rather than a function-wide binding.
        self.session
            .whitenoise()?
            .background_refresh_account_group_subscriptions(&account);

        Ok(())
    }

    async fn refresh_marmot_group_projection(
        &self,
        marmot_session: Arc<tokio::sync::Mutex<crate::marmot::session::MarmotSession>>,
        marmot_group_id: &MarmotGroupId,
    ) -> Result<()> {
        let projection = {
            let session = marmot_session.lock().await;
            session.group_projection(marmot_group_id)?
        };
        self.session
            .marmot_storage
            .put_group_projection(&projection)?;

        Ok(())
    }

    async fn marmot_publish_routes_for_group_creation(
        &self,
        plan: &MarmotGroupCreationPlan,
        group_id: &MarmotGroupId,
        account: &Account,
    ) -> Result<MarmotPublishRoutes> {
        let mut routes = MarmotPublishRoutes::new().with_group_publish_route(
            MarmotGroupPublishRoute::from_nostr_routing(group_id.clone(), plan.routing.clone()),
        );

        for resolved in &plan.resolved_members {
            let relays = self
                .session
                .shared
                .resolve_member_delivery_relays(
                    &resolved.user,
                    account,
                    "whitenoise::session::groups::create_marmot_group",
                )
                .await?;
            let endpoints = Relay::urls(&relays)
                .into_iter()
                .map(|relay| TransportEndpoint(relay.to_string()))
                .collect();
            routes = routes.with_inbox_route(
                MemberId::new(resolved.user.pubkey.as_bytes().to_vec()),
                endpoints,
            );
        }

        Ok(routes)
    }

    async fn marmot_publish_routes_for_member_addition(
        &self,
        group_id: &MarmotGroupId,
        resolved_members: &[MarmotResolvedMemberKeyPackage],
        account: &Account,
    ) -> Result<MarmotPublishRoutes> {
        let mut routes = self
            .marmot_publish_routes_for_existing_group(group_id)
            .await?;

        for resolved in resolved_members {
            let relays = self
                .session
                .shared
                .resolve_member_delivery_relays(
                    &resolved.user,
                    account,
                    "whitenoise::session::groups::add_marmot_members",
                )
                .await?;
            let endpoints = Relay::urls(&relays)
                .into_iter()
                .map(|relay| TransportEndpoint(relay.to_string()))
                .collect();
            routes = routes.with_inbox_route(
                MemberId::new(resolved.user.pubkey.as_bytes().to_vec()),
                endpoints,
            );
        }

        Ok(routes)
    }

    async fn marmot_publish_routes_for_existing_group(
        &self,
        group_id: &MarmotGroupId,
    ) -> Result<MarmotPublishRoutes> {
        let marmot_session = self.marmot_session()?;
        let group_route = {
            let session = marmot_session.lock().await;
            session.group_publish_route(group_id)?
        };

        Ok(MarmotPublishRoutes::new().with_group_publish_route(group_route))
    }

    pub(crate) async fn prepare_marmot_group_creation(
        &self,
        member_pubkeys: Vec<PublicKey>,
        config: GroupConfig,
    ) -> Result<MarmotGroupCreationPlan> {
        self.prepare_marmot_group_creation_with_nostr_group_id(
            member_pubkeys,
            config,
            random_nostr_group_id(),
        )
        .await
    }

    pub(crate) async fn prepare_marmot_group_creation_with_nostr_group_id(
        &self,
        member_pubkeys: Vec<PublicKey>,
        config: GroupConfig,
        nostr_group_id: [u8; 32],
    ) -> Result<MarmotGroupCreationPlan> {
        self.validate_marmot_group_create_members(&member_pubkeys, &config.admins)?;
        Self::validate_marmot_group_create_config(&config)?;
        let requested_member_pubkeys = member_pubkeys.clone();

        let member_futures = member_pubkeys
            .iter()
            .map(|pk| self.resolve_marmot_member_key_package(pk));
        let resolved_members = try_join_all(member_futures).await?;
        let key_packages = resolved_members
            .iter()
            .map(|resolved| resolved.key_package.clone())
            .collect();

        let (request, routing) =
            self.marmot_create_group_request(member_pubkeys, key_packages, config, nostr_group_id)?;

        Ok(MarmotGroupCreationPlan {
            request,
            routing,
            member_pubkeys: requested_member_pubkeys,
            resolved_members,
        })
    }

    fn marmot_create_group_request(
        &self,
        member_pubkeys: Vec<PublicKey>,
        key_packages: Vec<KeyPackage>,
        config: GroupConfig,
        nostr_group_id: [u8; 32],
    ) -> Result<(CreateGroupRequest, NostrRoutingV1)> {
        let routing = NostrRoutingV1::new(
            nostr_group_id,
            config.relays.iter().map(ToString::to_string).collect(),
        )
        .map_err(|error| {
            WhitenoiseError::InvalidInput(format!(
                "invalid marmot.transport.nostr.routing.v1: {error}"
            ))
        })?;
        let routing_component = AppComponentData {
            component_id: NOSTR_ROUTING_COMPONENT_ID,
            data: encode_nostr_routing_v1(&routing).map_err(|error| {
                WhitenoiseError::InvalidInput(format!(
                    "invalid marmot.transport.nostr.routing.v1: {error}"
                ))
            })?,
        };
        let mut app_components = vec![routing_component];

        if let Some(seconds) = config.disappearing_message_secs {
            if seconds == 0 {
                return Err(WhitenoiseError::InvalidInput(
                    "disappearing_message_secs must be greater than zero".to_string(),
                ));
            }
            app_components.push(AppComponentData {
                component_id: GROUP_MESSAGE_RETENTION_COMPONENT_ID,
                data: seconds.to_be_bytes().to_vec(),
            });
        }

        app_components.push(
            AgentTextStreamQuicPolicyV1::user_to_agent_default()
                .to_app_component_data()
                .map_err(|error| {
                    WhitenoiseError::InvalidInput(format!(
                        "invalid agent text stream policy: {error}"
                    ))
                })?,
        );

        Ok((
            CreateGroupRequest {
                name: config.name,
                description: config.description,
                members: key_packages,
                required_features: Vec::new(),
                app_components,
                initial_admins: self.marmot_initial_admins(&member_pubkeys, &config.admins),
            },
            routing,
        ))
    }

    fn validate_marmot_group_create_members(
        &self,
        member_pubkeys: &[PublicKey],
        admin_pubkeys: &[PublicKey],
    ) -> Result<()> {
        let unique_members: BTreeSet<&PublicKey> = member_pubkeys.iter().collect();
        if unique_members.len() != member_pubkeys.len() {
            return Err(WhitenoiseError::InvalidInput(
                "member_pubkeys contains duplicates".to_string(),
            ));
        }

        if !admin_pubkeys.contains(&self.session.account_pubkey) {
            return Err(WhitenoiseError::InvalidInput(
                "creator must be an admin".to_string(),
            ));
        }

        if member_pubkeys.contains(&self.session.account_pubkey) {
            return Err(WhitenoiseError::InvalidInput(
                "creator must not be included as a member".to_string(),
            ));
        }

        for admin in admin_pubkeys {
            if *admin != self.session.account_pubkey && !member_pubkeys.contains(admin) {
                return Err(WhitenoiseError::InvalidInput(
                    "admin must be a member".to_string(),
                ));
            }
        }

        Ok(())
    }

    async fn prepare_marmot_member_addition(
        &self,
        member_pubkeys: Vec<PublicKey>,
    ) -> Result<Vec<MarmotResolvedMemberKeyPackage>> {
        let unique_members: BTreeSet<&PublicKey> = member_pubkeys.iter().collect();
        if unique_members.len() != member_pubkeys.len() {
            return Err(WhitenoiseError::InvalidInput(
                "member_pubkeys contains duplicates".to_string(),
            ));
        }

        let member_futures = member_pubkeys
            .iter()
            .map(|pk| self.resolve_marmot_member_key_package(pk));
        try_join_all(member_futures).await
    }

    fn validate_marmot_group_create_config(config: &GroupConfig) -> Result<()> {
        if Self::requires_legacy_group_image_create(config) {
            return Err(WhitenoiseError::InvalidInput(
                "Darkmatter group image creation requires image upload key and media type support"
                    .to_string(),
            ));
        }

        Ok(())
    }

    fn requires_legacy_group_image_create(config: &GroupConfig) -> bool {
        config.image_hash.is_some() || config.image_key.is_some() || config.image_nonce.is_some()
    }

    fn marmot_initial_admins(
        &self,
        member_pubkeys: &[PublicKey],
        admin_pubkeys: &[PublicKey],
    ) -> Vec<MemberId> {
        let member_set: BTreeSet<PublicKey> = member_pubkeys.iter().copied().collect();
        let admin_set: BTreeSet<PublicKey> = admin_pubkeys.iter().copied().collect();

        admin_set
            .into_iter()
            .filter(|admin| *admin != self.session.account_pubkey)
            .filter(|admin| member_set.contains(admin))
            .map(|admin| MemberId::new(admin.as_bytes().to_vec()))
            .collect()
    }

    pub(crate) async fn project_created_marmot_group(
        &self,
        group: &MarmotCreatedGroupProjection,
        member_pubkeys: &[PublicKey],
        group_type: Option<GroupType>,
    ) -> Result<GroupId> {
        let group_id = GroupId::from_slice(group.group_id.as_slice());
        self.session.marmot_storage.put_group_projection(group)?;
        self.finalize_group_records(&group_id, member_pubkeys, group_type, &group.name)
            .await?;

        Ok(group_id)
    }

    pub(crate) async fn project_joined_marmot_group(
        &self,
        group: &MarmotCreatedGroupProjection,
        group_type: Option<GroupType>,
        welcomer_pubkey: PublicKey,
    ) -> Result<GroupId> {
        let group_id = GroupId::from_slice(group.group_id.as_slice());
        let group_type = group_type
            .unwrap_or_else(|| GroupInformation::infer_group_type_from_group_name(&group.name));
        let dm_peer_pubkey = if group_type == GroupType::DirectMessage {
            Some(welcomer_pubkey)
        } else {
            None
        };

        self.session.marmot_storage.put_group_projection(group)?;
        let (_group_info, _was_created) = GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(group_type),
            &self.session.shared.database,
        )
        .await?;

        let now = Utc::now();
        let account_group = AccountGroup {
            id: None,
            account_pubkey: self.session.account_pubkey,
            mls_group_id: group_id.clone(),
            user_confirmation: None,
            welcomer_pubkey: Some(welcomer_pubkey),
            last_read_message_id: None,
            pin_order: None,
            dm_peer_pubkey,
            archived_at: None,
            removed_at: None,
            self_removed: false,
            muted_until: None,
            chat_cleared_at: None,
            created_at: now,
            updated_at: now,
        };
        account_group
            .save(&self.session.account_db.inner.pool)
            .await?;

        Ok(group_id)
    }

    pub(crate) async fn resolve_marmot_member_key_package(
        &self,
        pk: &PublicKey,
    ) -> Result<MarmotResolvedMemberKeyPackage> {
        let shared = &self.session.shared;
        let (user, created) = User::find_or_create_by_pubkey(pk, &shared.database).await?;
        if created && let Err(e) = user.update_relay_lists(shared).await {
            tracing::warn!(
                target: "whitenoise::session::groups",
                "Failed to update relay lists for new user {}: {}",
                user.pubkey,
                e
            );
        }

        let lookup = user.key_package_lookup(shared).await?;
        let event = Self::marmot_member_key_package_from_lookup(*pk, lookup)?;
        let key_package = key_package_from_v2_event(&event)?;

        Ok(MarmotResolvedMemberKeyPackage {
            user,
            event,
            key_package,
        })
    }

    fn marmot_member_key_package_from_lookup(
        member_pubkey: PublicKey,
        lookup: KeyPackageLookup,
    ) -> Result<Event> {
        match lookup {
            KeyPackageLookup::Found(event) => Ok(event),
            KeyPackageLookup::NotFound => {
                Err(WhitenoiseError::MemberKeyPackageNotFound { member_pubkey })
            }
            KeyPackageLookup::Incompatible => Err(WhitenoiseError::UnsupportedKeyPackageFormat {
                member_pubkey,
                reason: "no Darkmatter v2-compatible package was available".to_string(),
            }),
        }
    }

    /// Creates local database records for a newly created group:
    /// GroupInformation and AccountGroup (auto-accepted for the creator).
    async fn finalize_group_records(
        &self,
        group_id: &GroupId,
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
            group_id,
            Some(group_type),
            &self.session.shared.database,
        )
        .await?;

        let (account_group, _) = AccountGroup::find_or_create(
            &self.session.account_pubkey,
            group_id,
            dm_peer,
            &self.session.account_db.inner.pool,
        )
        .await?;
        account_group
            .update_user_confirmation(true, &self.session.account_db.inner.pool)
            .await?;

        // Best-effort: share the creator's push token into the new group.
        if let Err(error) = self
            .session
            .push()
            .share_local_token_to_group(group_id)
            .await
        {
            tracing::warn!(
                target: "whitenoise::session::groups",
                account = %self.session.account_pubkey.to_hex(),
                group = %hex::encode(group_id.as_slice()),
                error = %error,
                "Failed to share local push token after group creation"
            );
        }

        Ok(())
    }

    /// Return a view for group media operations scoped to this session.
    pub fn media(&self) -> MediaOps<'_> {
        MediaOps::new(self.session)
    }
}

fn random_nostr_group_id() -> [u8; 32] {
    let mut id = [0_u8; 32];
    ::rand::rng().fill_bytes(&mut id);
    id
}

#[cfg(test)]
mod tests {
    use cgka_traits::storage::GroupStorage;

    use super::*;
    use crate::whitenoise::session::test_helpers::test_session_with_marmot_keys;
    use crate::whitenoise::test_utils::{
        ObsoleteMlsArtifacts, assert_obsolete_mls_artifacts_absent, create_mock_whitenoise,
        setup_unprojected_accepted_group, wait_for_key_package_publication,
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
    async fn public_create_group_uses_darkmatter_for_supported_config() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member]).await;

        let session = whitenoise.require_session(&creator.pubkey).unwrap();
        let config = GroupConfig::new(
            "public darkmatter group".to_string(),
            "public create_group should use Darkmatter".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("ws://localhost:7777").unwrap()],
            vec![creator.pubkey],
            None,
        );

        let group = session
            .groups()
            .create_group(vec![member.pubkey], config, None)
            .await
            .unwrap();

        assert_obsolete_mls_artifacts_absent_for_session(&session);

        let marmot_group_id = MarmotGroupId::new(group.mls_group_id.as_slice().to_vec());
        assert!(session.marmot_storage.get_group(&marmot_group_id).is_ok());
        assert!(
            session
                .marmot_storage
                .find_group_projection(&marmot_group_id)
                .unwrap()
                .is_some(),
            "public create_group must project confirmed Darkmatter groups"
        );
    }

    #[tokio::test]
    async fn public_create_group_rejects_image_config_until_darkmatter_media_exists() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member]).await;

        let session = whitenoise.require_session(&creator.pubkey).unwrap();
        let config = GroupConfig::new(
            "image-backed group".to_string(),
            "image metadata needs the Darkmatter image component".to_string(),
            Some([1; 32]),
            Some([2; 32]),
            Some([3; 12]),
            vec![RelayUrl::parse("ws://localhost:7777").unwrap()],
            vec![creator.pubkey],
            None,
        );

        let result = session
            .groups()
            .create_group(vec![member.pubkey], config, None)
            .await;

        match result {
            Err(WhitenoiseError::InvalidInput(message)) => {
                assert!(message.contains("Darkmatter group image creation"))
            }
            other => panic!("expected unsupported Darkmatter image creation error, got {other:?}"),
        }
        assert!(
            session.groups().all(false).unwrap().is_empty(),
            "unsupported image group creation must not create a local group"
        );
    }

    #[tokio::test]
    async fn relays_use_darkmatter_projection_without_obsolete_mls_storage() {
        let keys = Keys::generate();
        let session = test_session_with_marmot_keys(keys.clone()).await;
        let marmot_group_id = MarmotGroupId::new(vec![0xB1; 32]);
        let group_id = GroupId::from(&marmot_group_id);

        session
            .marmot_storage
            .put_group_projection(&MarmotCreatedGroupProjection {
                group_id: marmot_group_id,
                name: "projected relays".to_string(),
                description: "relays should not open obsolete MLS storage".to_string(),
                epoch: 1,
                routing: NostrRoutingV1::new(
                    [0xB2; 32],
                    vec![
                        "wss://relay-b.example".to_string(),
                        "wss://relay-a.example".to_string(),
                    ],
                )
                .unwrap(),
                admin_pubkeys: BTreeSet::from([keys.public_key()]),
                member_pubkeys: BTreeSet::from([keys.public_key()]),
                self_update_completed_at_secs: 1,
                disappearing_message_secs: None,
            })
            .unwrap();

        let relays = session.groups().relays(&group_id).unwrap();

        assert_eq!(
            relays,
            BTreeSet::from([
                RelayUrl::parse("wss://relay-a.example").unwrap(),
                RelayUrl::parse("wss://relay-b.example").unwrap(),
            ])
        );
        assert_obsolete_mls_artifacts_absent_for_session(&session);
    }

    #[tokio::test]
    async fn session_group_reads_skip_unprojected_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[0xC7; 32]);
        let session = whitenoise.require_session(&creator.pubkey).unwrap();
        session
            .membership()
            .for_group(&group_id)
            .get_or_create(None)
            .await
            .unwrap();
        assert_obsolete_mls_artifacts_absent_for_session(&session);

        assert!(matches!(
            session.groups().get(&group_id),
            Err(WhitenoiseError::GroupNotFound)
        ));
        assert!(matches!(
            session.groups().members(&group_id),
            Err(WhitenoiseError::GroupNotFound)
        ));
        assert!(matches!(
            session.groups().relays(&group_id),
            Err(WhitenoiseError::GroupNotFound)
        ));
        assert!(
            session
                .groups()
                .all(false)
                .unwrap()
                .into_iter()
                .all(|group| group.mls_group_id != group_id),
            "unprojected groups must not be returned by session group lists"
        );
        assert!(
            session
                .groups()
                .visible()
                .await
                .unwrap()
                .into_iter()
                .all(|group| group.group.mls_group_id != group_id),
            "unprojected groups must not be visible through account membership"
        );
        assert_obsolete_mls_artifacts_absent_for_session(&session);
    }

    #[tokio::test]
    async fn add_members_rejects_unprojected_group_without_key_package_lookup() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();
        let new_member = whitenoise.create_identity().await.unwrap();

        let group_id = setup_unprojected_accepted_group(&whitenoise, &[&creator, &member]).await;
        let session = whitenoise.require_session(&creator.pubkey).unwrap();

        let result = session
            .groups()
            .add_members(&group_id, vec![new_member.pubkey])
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn remove_members_rejects_unprojected_group_without_mutation() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let group_id = setup_unprojected_accepted_group(&whitenoise, &[&creator, &member]).await;
        let session = whitenoise.require_session(&creator.pubkey).unwrap();

        let result = session
            .groups()
            .remove_members(&group_id, vec![member.pubkey])
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn update_group_data_rejects_unprojected_group_without_mutation() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let group_id = setup_unprojected_accepted_group(&whitenoise, &[&creator, &member]).await;
        let session = whitenoise.require_session(&creator.pubkey).unwrap();
        let update = GroupDataUpdate {
            name: Some("darkmatter only".to_string()),
            description: None,
            image_hash: None,
            image_key: None,
            image_nonce: None,
            image_upload_key: None,
            admins: None,
            relays: None,
            nostr_group_id: None,
            disappearing_message_secs: None,
        };

        let result = session.groups().update_group_data(&group_id, update).await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn self_demote_rejects_unprojected_group_without_mutation() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let group_id = setup_unprojected_accepted_group(&whitenoise, &[&creator, &member]).await;
        let session = whitenoise.require_session(&creator.pubkey).unwrap();

        let result = session.groups().self_demote(&group_id).await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }

    #[tokio::test]
    async fn leave_rejects_unprojected_group_without_mutation() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let group_id = setup_unprojected_accepted_group(&whitenoise, &[&creator, &member]).await;
        let session = whitenoise.require_session(&member.pubkey).unwrap();

        let result = session.groups().leave(&group_id).await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }
}

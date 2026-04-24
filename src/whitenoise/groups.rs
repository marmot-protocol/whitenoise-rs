use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use futures::future::{join_all, try_join_all};
use mdk_core::prelude::*;
use nostr_sdk::prelude::*;

use crate::{
    RelayType, perf_instrument, perf_span,
    relay_control::ephemeral::KeyPackageLookup,
    whitenoise::{
        Whitenoise,
        accounts::Account,
        accounts_groups::AccountGroup,
        error::{Result, WhitenoiseError},
        group_information::{GroupInformation, GroupType},
        key_packages::{REQUIRED_MLS_PROPOSAL_TAGS, validate_fetched_member_key_package},
        relays::Relay,
        users::User,
    },
};

pub mod blossom_error;
mod media;
mod membership;
mod publish;
mod required_proposals;

pub use membership::{GroupWithInfoAndMembership, GroupWithMembership};
pub use required_proposals::RequiredProposal;

impl Whitenoise {
    #[perf_instrument("groups")]
    pub(crate) async fn resolve_member_delivery_relays(
        &self,
        member: &User,
        fallback_account: &Account,
        context: &'static str,
    ) -> Result<Vec<Relay>> {
        let inbox_relays = member
            .relays(RelayType::Inbox, &self.shared.database)
            .await?;
        if !inbox_relays.is_empty() {
            return Ok(inbox_relays);
        }

        let nip65_relays = member
            .relays(RelayType::Nip65, &self.shared.database)
            .await?;
        if !nip65_relays.is_empty() {
            return Ok(nip65_relays);
        }

        let fallback_relays = fallback_account.nip65_relays(self).await?;
        if fallback_relays.is_empty() {
            tracing::error!(
                target: "whitenoise::accounts::groups::relay_selection",
                context = context,
                "User {} has no inbox or NIP-65 relays and account {} has no fallback relays configured",
                member.pubkey,
                fallback_account.pubkey
            );
            return Err(WhitenoiseError::MissingWelcomeRelays {
                member_pubkey: member.pubkey,
                account_pubkey: fallback_account.pubkey,
            });
        } else {
            tracing::warn!(
                target: "whitenoise::accounts::groups::relay_selection",
                context = context,
                "User {} has no inbox or NIP-65 relays, using account {} fallback relays",
                member.pubkey,
                fallback_account.pubkey
            );
        }

        Ok(fallback_relays)
    }

    /// Resolves a single member for group creation: finds or creates the user record,
    /// syncs relay lists for new users, fetches and validates the key package.
    async fn resolve_member_key_package(&self, pk: &PublicKey) -> Result<(User, Event)> {
        let (user, created) = User::find_or_create_by_pubkey(pk, &self.shared.database).await?;
        if created && let Err(e) = user.update_relay_lists(self).await {
            tracing::warn!(
                target: "whitenoise::groups",
                "Failed to update relay lists for new user {}: {}",
                user.pubkey,
                e
            );
        }

        let _kp_fetch = perf_span!("groups::fetch_key_package");
        let lookup = user.key_package_lookup(self).await?;
        drop(_kp_fetch);

        let event = match lookup {
            KeyPackageLookup::Found(event) => event,
            KeyPackageLookup::Incompatible { error } => {
                if let WhitenoiseError::MissingMlsProposals { missing } = &error {
                    let missing_self_remove = missing
                        .iter()
                        .any(|proposal| proposal == REQUIRED_MLS_PROPOSAL_TAGS[0]);
                    if missing_self_remove {
                        return Err(WhitenoiseError::KeyPackageMissingSelfRemove {
                            member_pubkey: *pk,
                        });
                    }
                }

                return Err(WhitenoiseError::IncompatibleKeyPackage {
                    member_pubkey: *pk,
                    reason: error.to_string(),
                });
            }
            KeyPackageLookup::NotFound => {
                return Err(WhitenoiseError::MdkCoreError(mdk_core::Error::KeyPackage(
                    "Does not exist".to_owned(),
                )));
            }
        };

        validate_fetched_member_key_package(&event, pk)?;

        Ok((user, event))
    }

    /// Creates local database records for a newly created group.
    #[allow(deprecated)]
    #[perf_instrument("groups")]
    async fn finalize_group_records(
        &self,
        group: &group_types::Group,
        member_pubkeys: &[PublicKey],
        group_type: Option<GroupType>,
        group_name: &str,
        creator_account: &Account,
    ) -> Result<()> {
        let group_info = GroupInformation::create_for_group(
            self,
            &group.mls_group_id.clone(),
            group_type,
            group_name,
        )
        .await?;

        let dm_peer = if group_info.group_type == GroupType::DirectMessage {
            member_pubkeys.first()
        } else {
            None
        };

        let (account_group, _) = AccountGroup::get_or_create(
            self,
            &creator_account.pubkey,
            &group.mls_group_id,
            dm_peer,
        )
        .await?;
        account_group.accept(self).await?;

        if let Err(error) = self
            .share_local_push_token_to_group(creator_account, &group.mls_group_id)
            .await
        {
            tracing::warn!(
                target: "whitenoise::groups",
                account = %creator_account.pubkey.to_hex(),
                group = %hex::encode(group.mls_group_id.as_slice()),
                error = %error,
                "Failed to share local push token after group creation"
            );
        }

        Ok(())
    }

    // NOTE: Unlike the other deprecated wrappers below (all(), get(), archive_chat(), etc.),
    // this method retains its own implementation rather than delegating to
    // `AccountSession::groups().create_group()`. The session path calls `Self::wn()` (the
    // singleton bridge) which panics in unit tests where no global singleton is registered.
    // Delegating here would break the existing test suite. Remove this copy when Phase 16
    // eliminates the singleton.
    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().create_group() instead."
    )]
    #[allow(deprecated)]
    #[perf_instrument("groups")]
    pub async fn create_group(
        &self,
        creator_account: &Account,
        member_pubkeys: Vec<PublicKey>,
        config: NostrGroupConfigData,
        group_type: Option<GroupType>,
    ) -> Result<group_types::Group> {
        let signer = self.get_signer_for_account(creator_account)?;

        let unique: BTreeSet<&PublicKey> = member_pubkeys.iter().collect();
        if unique.len() != member_pubkeys.len() {
            return Err(WhitenoiseError::InvalidInput(
                "member_pubkeys contains duplicates".to_string(),
            ));
        }

        let member_futures = member_pubkeys
            .iter()
            .map(|pk| self.resolve_member_key_package(pk));
        let resolved_members = try_join_all(member_futures).await?;
        let (members, key_package_events): (Vec<User>, Vec<Event>) =
            resolved_members.into_iter().unzip();

        let mdk = self.create_mdk_for_account(creator_account.pubkey)?;
        let group_name = config.name.clone();

        let create_group_result =
            mdk.create_group(&creator_account.pubkey, key_package_events.clone(), config)?;

        let group = create_group_result.group;

        if create_group_result.welcome_rumors.len() != members.len() {
            return Err(WhitenoiseError::Internal(
                "Welcome rumours are missing for some of the members".to_string(),
            ));
        }

        let kp_pubkey_by_event_id: std::collections::HashMap<EventId, PublicKey> =
            key_package_events
                .iter()
                .map(|event| (event.id, event.pubkey))
                .collect();

        let mut members_by_pubkey: std::collections::HashMap<PublicKey, User> = members
            .into_iter()
            .map(|member| (member.pubkey, member))
            .collect();

        let welcome_data: Vec<(UnsignedEvent, User, PublicKey)> = create_group_result
            .welcome_rumors
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
            .collect::<Result<_>>()?;

        self.finalize_group_records(
            &group,
            &member_pubkeys,
            group_type,
            &group_name,
            creator_account,
        )
        .await?;

        // Background welcome publishing
        let creator_account_clone = creator_account.clone();
        let whitenoise = self.arc()?;
        tokio::spawn(async move {
            let futures = welcome_data
                .into_iter()
                .map(|(rumor, member, member_pubkey)| {
                    let signer: Arc<dyn NostrSigner> = signer.clone();
                    let creator = &creator_account_clone;
                    let whitenoise = &whitenoise;
                    async move {
                        let relays_to_use = whitenoise
                            .resolve_member_delivery_relays(
                                &member,
                                creator,
                                "whitenoise::groups::create_group",
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
                        target: "whitenoise::groups",
                        "Background welcome publish failed: {}",
                        error
                    );
                }
            }
        });

        self.background_refresh_account_group_subscriptions(creator_account);

        Ok(group)
    }

    #[deprecated(since = "0.0.0", note = "Use AccountSession::groups().all() instead.")]
    pub async fn groups(
        &self,
        account: &Account,
        active_filter: bool,
    ) -> Result<Vec<group_types::Group>> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session.groups().all(active_filter)
    }

    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().visible() instead."
    )]
    pub async fn visible_groups(&self, account: &Account) -> Result<Vec<GroupWithMembership>> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session.groups().visible().await
    }

    #[deprecated(since = "0.0.0", note = "Use AccountSession::groups().get() instead.")]
    pub async fn group(&self, account: &Account, group_id: &GroupId) -> Result<group_types::Group> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session.groups().get(group_id)
    }

    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().members() instead."
    )]
    pub async fn group_members(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<Vec<PublicKey>> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session.groups().members(group_id)
    }

    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().relays() instead."
    )]
    pub async fn group_relays(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<BTreeSet<RelayUrl>> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session.groups().relays(group_id)
    }

    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().admins() instead."
    )]
    pub async fn group_admins(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<Vec<PublicKey>> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session.groups().admins(group_id)
    }

    /// Returns the set of MLS proposal types required by the group's
    /// `RequiredCapabilities` extension, projected onto the whitenoise mirror
    /// enum [`RequiredProposal`].
    ///
    /// # Arguments
    /// * `account` - The account that has access to the group
    /// * `group_id` - The MLS group ID to inspect
    ///
    /// # Returns
    /// * `Ok(BTreeSet<RequiredProposal>)` - The set of required proposal
    ///   types. An empty set is the LCD outcome for mixed or empty-invitee
    ///   groups and is **not** an error — it is distinct from
    ///   [`WhitenoiseError::GroupNotFound`], which means the MLS record is
    ///   missing.
    ///
    /// # Errors
    /// * `WhitenoiseError::GroupNotFound` - If no MLS record exists for
    ///   `group_id`
    /// * `WhitenoiseError` - If loading the group record fails
    ///
    /// # Examples
    /// ```ignore
    /// let required = wn.group_required_proposals(&account, &group_id).await?;
    /// if required.contains(&RequiredProposal::SelfRemove) {
    ///     // Non-admin members can leave without an admin commit.
    /// }
    /// ```
    #[perf_instrument("groups")]
    pub async fn group_required_proposals(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<BTreeSet<RequiredProposal>> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        let required = mdk
            .group_required_proposals(group_id)
            .map_err(|e| match e {
                mdk_core::Error::GroupNotFound => WhitenoiseError::GroupNotFound,
                other => WhitenoiseError::from(other),
            })?;
        Ok(required.into_iter().map(RequiredProposal::from).collect())
    }

    #[allow(deprecated)]
    #[perf_instrument("groups")]
    async fn ensure_account_is_group_admin(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<()> {
        let admins = self.group_admins(account, group_id).await?;
        if !admins.contains(&account.pubkey) {
            return Err(WhitenoiseError::AccountNotAuthorized);
        }

        Ok(())
    }

    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().add_members() instead."
    )]
    #[allow(deprecated)]
    #[perf_instrument("groups")]
    pub async fn add_members_to_group(
        &self,
        account: &Account,
        group_id: &GroupId,
        members: Vec<PublicKey>,
    ) -> Result<()> {
        self.ensure_account_is_group_admin(account, group_id)
            .await?;

        let mut key_package_events: Vec<Event> = Vec::new();
        let signer = self.get_signer_for_account(account)?;
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        let mut users = Vec::new();

        for pk in members.iter() {
            let (user, event) = self.resolve_member_key_package(pk).await?;
            key_package_events.push(event);
            users.push(user);
        }

        let relay_urls = Self::ensure_group_relays(&mdk, group_id)?;

        let _mls_add = perf_span!("groups::mls_add_members");
        let update_result = mdk.add_members(group_id, &key_package_events)?;
        drop(_mls_add);

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

        self.publish_and_merge_commit(evolution_event, &account.pubkey, group_id, &relay_urls)
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

            let relays_to_use = self
                .resolve_member_delivery_relays(
                    &user,
                    account,
                    "whitenoise::accounts::groups::add_members_to_group",
                )
                .await?;

            let relay_urls = Relay::urls(&relays_to_use);

            self.shared
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

    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().remove_members() instead."
    )]
    #[allow(deprecated)]
    #[perf_instrument("groups")]
    pub async fn remove_members_from_group(
        &self,
        account: &Account,
        group_id: &GroupId,
        members: Vec<PublicKey>,
    ) -> Result<()> {
        self.ensure_account_is_group_admin(account, group_id)
            .await?;

        let (relay_urls, evolution_event) = {
            let mdk = self.create_mdk_for_account(account.pubkey)?;
            let relay_urls = Self::ensure_group_relays(&mdk, group_id)?;
            let update_result = mdk.remove_members(group_id, &members)?;
            (relay_urls, update_result.evolution_event)
        };

        self.publish_and_merge_commit(evolution_event, &account.pubkey, group_id, &relay_urls)
            .await
    }

    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().update_group_data() instead."
    )]
    #[allow(deprecated)]
    #[perf_instrument("groups")]
    pub async fn update_group_data(
        &self,
        account: &Account,
        group_id: &GroupId,
        group_data: NostrGroupDataUpdate,
    ) -> Result<()> {
        self.ensure_account_is_group_admin(account, group_id)
            .await?;

        let (relay_urls, evolution_event) = {
            let mdk = self.create_mdk_for_account(account.pubkey)?;
            let relay_urls = Self::ensure_group_relays(&mdk, group_id)?;
            let update_result = mdk.update_group_data(group_id, group_data)?;
            (relay_urls, update_result.evolution_event)
        };

        self.publish_and_merge_commit(evolution_event, &account.pubkey, group_id, &relay_urls)
            .await?;
        self.background_refresh_account_group_subscriptions(account);
        Ok(())
    }

    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().self_demote() instead."
    )]
    #[allow(deprecated)]
    #[perf_instrument("groups")]
    pub async fn self_demote(&self, account: &Account, group_id: &GroupId) -> Result<()> {
        self.ensure_account_is_group_admin(account, group_id)
            .await?;

        let (relay_urls, evolution_event) = {
            let mdk = self.create_mdk_for_account(account.pubkey)?;
            let relay_urls = Self::ensure_group_relays(&mdk, group_id)?;
            let update_result = mdk.self_demote(group_id)?;
            (relay_urls, update_result.evolution_event)
        };

        self.publish_and_merge_commit(evolution_event, &account.pubkey, group_id, &relay_urls)
            .await
    }

    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().leave() instead."
    )]
    #[allow(deprecated)]
    #[perf_instrument("groups")]
    pub async fn leave_group(&self, account: &Account, group_id: &GroupId) -> Result<()> {
        let account_group = AccountGroup::get(self, &account.pubkey, group_id)
            .await?
            .ok_or(WhitenoiseError::GroupNotFound)?;

        if account_group.is_removed() {
            return Err(WhitenoiseError::AlreadyDepartedFromGroup);
        }

        let (relay_urls, evolution_event) = {
            let mdk = self.create_mdk_for_account(account.pubkey)?;
            let relay_urls = Self::ensure_group_relays(&mdk, group_id)?;
            let update_result = mdk.leave_group(group_id)?;
            (relay_urls, update_result.evolution_event)
        };

        self.publish_event_with_retry(evolution_event, &account.pubkey, &relay_urls)
            .await?;

        // Optimistic local update is best-effort after publish success.
        // When the auto-committed removal commit arrives later,
        // mark_as_removed() will converge the local state regardless.
        #[allow(deprecated)]
        if let Err(error) = self.mark_as_left(account, group_id).await {
            tracing::warn!(
                target: "whitenoise::groups",
                account_pubkey = %account.pubkey,
                group_id = %hex::encode(group_id.as_slice()),
                "SelfRemove published but failed to mark local departure: {error}",
            );
        }

        self.background_refresh_account_group_subscriptions(account);

        Ok(())
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;
    use crate::whitenoise::Whitenoise;
    use crate::whitenoise::database::media_files::MediaFile;
    use crate::whitenoise::test_utils::*;
    use mdk_core::media_processing::MediaProcessingOptions;
    use mdk_storage_traits::Secret;
    use nostr_blossom::bud02::BlobDescriptor;
    use nostr_sdk::RelayUrl;
    use nostr_sdk::prelude::hashes::Hash as _;
    use nostr_sdk::prelude::hashes::sha256::Hash as Sha256Hash;

    fn mock_blossom_url(server: &mockito::Server) -> Url {
        let socket_address = server.socket_address();
        Url::parse(&format!("http://{socket_address}")).unwrap()
    }

    fn mock_blob_descriptor(server_url: &Url, blob: &[u8], mime_type: &str) -> BlobDescriptor {
        let sha256 = Sha256Hash::hash(blob);
        let url = server_url.join(&sha256.to_string()).unwrap();

        BlobDescriptor {
            url,
            sha256,
            size: blob.len().try_into().unwrap(),
            mime_type: Some(mime_type.to_string()),
            uploaded: Timestamp::now(),
        }
    }

    #[tokio::test]
    async fn test_create_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Setup creator account
        let creator_account = whitenoise.create_identity().await.unwrap();

        // Setup member accounts
        let mut member_pubkeys = Vec::new();
        for _ in 0..2 {
            let member_account = whitenoise.create_identity().await.unwrap();
            let member_user =
                User::find_by_pubkey(&member_account.pubkey, &whitenoise.shared.database)
                    .await
                    .unwrap();
            creator_account
                .follow_user(&member_user, &whitenoise.shared.database)
                .await
                .unwrap();
            member_pubkeys.push(member_account.pubkey);
        }

        // Setup admin accounts (creator + one member as admin)
        let admin_pubkeys = vec![creator_account.pubkey, member_pubkeys[0]];

        // Test for success case
        case_create_group_success(
            &whitenoise,
            &creator_account,
            member_pubkeys.clone(),
            admin_pubkeys.clone(),
        )
        .await;

        // Test case: Empty admin list
        case_create_group_empty_admin_list(
            &whitenoise,
            &creator_account,
            member_pubkeys.clone(),
            vec![], // Empty admin list
        )
        .await;

        // Test case: Invalid admin pubkey (not a member)
        let non_member_pubkey = create_test_keys().public_key();
        case_create_group_invalid_admin_pubkey(
            &whitenoise,
            &creator_account,
            member_pubkeys.clone(),
            vec![creator_account.pubkey, non_member_pubkey],
        )
        .await;

        // Test case: DirectMessage group (2 participants total)
        case_create_direct_message_group(
            &whitenoise,
            &creator_account,
            vec![member_pubkeys[0]], // Only one member for DM
            vec![creator_account.pubkey, member_pubkeys[0]],
        )
        .await;
    }

    async fn case_create_group_success(
        whitenoise: &Whitenoise,
        creator_account: &Account,
        member_pubkeys: Vec<PublicKey>,
        admin_pubkeys: Vec<PublicKey>,
    ) {
        let config = create_nostr_group_config_data(admin_pubkeys.clone());
        // Create the group
        let result = whitenoise
            .create_group(
                creator_account,
                member_pubkeys.clone(),
                config.clone(),
                None,
            )
            .await;

        // Assert the group was created successfully
        assert!(result.is_ok(), "Error {:?}", result.unwrap_err());
        let group = result.unwrap();

        // Verify group metadata matches configuration
        assert_eq!(group.name, config.name);
        assert_eq!(group.description, config.description);
        assert_eq!(group.image_hash, config.image_hash);
        assert_eq!(group.image_key, config.image_key.map(Secret::new));

        // Verify admin configuration
        assert_eq!(group.admin_pubkeys.len(), admin_pubkeys.len());
        for admin_pk in &admin_pubkeys {
            assert!(
                group.admin_pubkeys.contains(admin_pk),
                "Admin {} not found in group.admin_pubkeys",
                admin_pk
            );
        }

        // Verify group state and type
        // Just check that group is in a valid state (we can't verify exact state without knowing the enum path)

        // Verify group information was created properly
        let group_info = GroupInformation::get_by_mls_group_id(
            creator_account.pubkey,
            &group.mls_group_id,
            whitenoise,
        )
        .await
        .unwrap();
        assert_eq!(group_info.mls_group_id, group.mls_group_id);
        assert_eq!(
            group_info.group_type,
            crate::whitenoise::group_information::GroupType::Group
        );
        // Note: participant_count is stored separately and managed by the GroupInformation logic

        // Verify group members can be retrieved
        let members = whitenoise
            .group_members(creator_account, &group.mls_group_id)
            .await
            .unwrap();
        assert_eq!(members.len(), member_pubkeys.len() + 1); // +1 for creator
        assert!(
            members.contains(&creator_account.pubkey),
            "Creator not in member list"
        );
        for member_pk in &member_pubkeys {
            assert!(
                members.contains(member_pk),
                "Member {} not found in group",
                member_pk
            );
        }

        // Verify group admins can be retrieved
        let admins = whitenoise
            .group_admins(creator_account, &group.mls_group_id)
            .await
            .unwrap();
        assert_eq!(admins.len(), admin_pubkeys.len());
        for admin_pk in &admin_pubkeys {
            assert!(
                admins.contains(admin_pk),
                "Admin {} not found in admin list",
                admin_pk
            );
        }

        // Verify AccountGroup was created and auto-accepted for the creator
        let account_group =
            AccountGroup::get(whitenoise, &creator_account.pubkey, &group.mls_group_id)
                .await
                .unwrap();
        assert!(
            account_group.is_some(),
            "AccountGroup should be created for creator"
        );
        let account_group = account_group.unwrap();
        assert!(
            account_group.is_accepted(),
            "AccountGroup should be auto-accepted for creator"
        );
    }

    /// Test case: Member/admin validation fails - empty admin list
    async fn case_create_group_empty_admin_list(
        whitenoise: &Whitenoise,
        creator_account: &Account,
        member_pubkeys: Vec<PublicKey>,
        admin_pubkeys: Vec<PublicKey>,
    ) {
        let config = create_nostr_group_config_data(admin_pubkeys.clone());
        let result = whitenoise
            .create_group(creator_account, member_pubkeys, config.clone(), None)
            .await;

        // Should fail because groups need at least one admin
        assert!(result.is_err());
        match result.unwrap_err() {
            WhitenoiseError::MdkCoreError(_) => {
                // Expected - invalid group configuration
            }
            other => panic!(
                "Expected NostrMlsError due to empty admin list, got: {:?}",
                other
            ),
        }
    }

    /// Test case: Key package fetching fails - invalid member pubkey
    async fn _case_create_group_key_package_fetch_fails(
        whitenoise: &Whitenoise,
        creator_account: &Account,
        member_pubkeys: Vec<PublicKey>,
        admin_pubkeys: Vec<PublicKey>,
    ) {
        let config = create_nostr_group_config_data(admin_pubkeys);
        let result = whitenoise
            .create_group(creator_account, member_pubkeys, config, None)
            .await;

        // Should fail because key package doesn't exist for the member
        assert!(result.is_err(), "{:?}", result);
    }

    /// Test case: Member/admin validation fails - non-existent admin
    async fn case_create_group_invalid_admin_pubkey(
        whitenoise: &Whitenoise,
        creator_account: &Account,
        member_pubkeys: Vec<PublicKey>,
        admin_pubkeys: Vec<PublicKey>,
    ) {
        let config = create_nostr_group_config_data(admin_pubkeys);
        let result = whitenoise
            .create_group(creator_account, member_pubkeys, config, None)
            .await;

        // Should fail because admin must be a member
        assert!(result.is_err());
        match result.unwrap_err() {
            WhitenoiseError::MdkCoreError(mdk_core::Error::Group(msg)) => {
                assert!(
                    msg.contains("Admin must be a member"),
                    "Expected 'Admin must be a member' error, got: {}",
                    msg
                );
            }
            other => panic!("Expected NostrMlsError::Group, got: {:?}", other),
        }
    }

    async fn case_create_direct_message_group(
        whitenoise: &Whitenoise,
        creator_account: &Account,
        member_pubkeys: Vec<PublicKey>,
        admin_pubkeys: Vec<PublicKey>,
    ) {
        // Direct message group should have exactly 1 member (plus creator = 2 total)
        assert_eq!(
            member_pubkeys.len(),
            1,
            "Direct message group should have exactly 1 member"
        );
        assert_eq!(
            admin_pubkeys.len(),
            2,
            "Direct message group should have 2 admins (both participants)"
        );

        let mut config = create_nostr_group_config_data(admin_pubkeys.clone());
        config.name = "".to_string();
        let result = whitenoise
            .create_group(creator_account, member_pubkeys.clone(), config, None)
            .await;

        assert!(result.is_ok(), "Error {:?}", result.unwrap_err());
        let group = result.unwrap();

        // Verify it's automatically classified as DirectMessage type
        let group_info = GroupInformation::get_by_mls_group_id(
            creator_account.pubkey,
            &group.mls_group_id,
            whitenoise,
        )
        .await
        .unwrap();
        assert_eq!(group_info.mls_group_id, group.mls_group_id);
        assert_eq!(
            group_info.group_type,
            crate::whitenoise::group_information::GroupType::DirectMessage
        );
        // DirectMessage groups should have exactly 2 participants (verified via member count below)

        // Verify both participants are admins (standard for DM groups)
        let admins = whitenoise
            .group_admins(creator_account, &group.mls_group_id)
            .await
            .unwrap();
        assert_eq!(admins.len(), 2, "DirectMessage group should have 2 admins");
        assert!(
            admins.contains(&creator_account.pubkey),
            "Creator should be admin"
        );
        assert!(
            admins.contains(&member_pubkeys[0]),
            "Member should be admin"
        );

        // Verify membership
        let members = whitenoise
            .group_members(creator_account, &group.mls_group_id)
            .await
            .unwrap();
        assert_eq!(
            members.len(),
            2,
            "DirectMessage group should have exactly 2 members"
        );
        assert!(
            members.contains(&creator_account.pubkey),
            "Creator should be member"
        );
        assert!(
            members.contains(&member_pubkeys[0]),
            "Member should be member"
        );
    }

    #[tokio::test]
    async fn test_group_member_management() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Setup creator and initial members
        let creator_account = whitenoise.create_identity().await.unwrap();
        let initial_members = setup_multiple_test_accounts(&whitenoise, 2).await;
        let initial_member_pubkeys = initial_members
            .iter()
            .map(|(acc, _)| acc.pubkey)
            .collect::<Vec<_>>();

        // Create group with initial members
        let admin_pubkeys = vec![creator_account.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys.clone());
        let group = whitenoise
            .create_group(
                &creator_account,
                initial_member_pubkeys.clone(),
                config,
                None,
            )
            .await
            .unwrap();

        // Verify initial membership
        let members = whitenoise
            .group_members(&creator_account, &group.mls_group_id)
            .await
            .unwrap();
        assert_eq!(members.len(), 3); // creator + 2 initial members

        // Add new members
        let new_members = setup_multiple_test_accounts(&whitenoise, 2).await;
        let new_member_pubkeys = new_members
            .iter()
            .map(|(acc, _)| acc.pubkey)
            .collect::<Vec<_>>();

        let add_result = whitenoise
            .add_members_to_group(
                &creator_account,
                &group.mls_group_id,
                new_member_pubkeys.clone(),
            )
            .await;
        assert!(
            add_result.is_ok(),
            "Failed to add members: {:?}",
            add_result.unwrap_err()
        );

        // Verify new membership count
        let updated_members = whitenoise
            .group_members(&creator_account, &group.mls_group_id)
            .await
            .unwrap();
        assert_eq!(updated_members.len(), 5); // creator + 2 initial + 2 new
        for new_member_pk in &new_member_pubkeys {
            assert!(
                updated_members.contains(new_member_pk),
                "New member {} not found",
                new_member_pk
            );
        }

        // Remove one member
        let member_to_remove = vec![initial_member_pubkeys[0]];
        let remove_result = whitenoise
            .remove_members_from_group(
                &creator_account,
                &group.mls_group_id,
                member_to_remove.clone(),
            )
            .await;
        assert!(
            remove_result.is_ok(),
            "Failed to remove member: {:?}",
            remove_result.unwrap_err()
        );

        // Verify final membership
        let final_members = whitenoise
            .group_members(&creator_account, &group.mls_group_id)
            .await
            .unwrap();
        assert_eq!(final_members.len(), 4); // creator + 1 remaining initial + 2 new
        assert!(
            !final_members.contains(&member_to_remove[0]),
            "Removed member still in group"
        );
    }

    #[tokio::test]
    async fn test_update_group_data() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Setup creator and member
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkeys = vec![members[0].0.pubkey];

        // Create group
        let admin_pubkeys = vec![creator_account.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys.clone());
        let group = whitenoise
            .create_group(&creator_account, member_pubkeys, config, None)
            .await
            .unwrap();

        // Update group data
        let new_group_data = NostrGroupDataUpdate {
            name: Some("Updated Group Name".to_string()),
            description: Some("Updated description".to_string()),
            image_hash: Some(Some([3u8; 32])), // 32-byte hash for new image
            image_key: Some(Some([4u8; 32])),  // 32-byte encryption key
            image_nonce: Some(Some([5u8; 12])), // 12-byte nonce
            image_upload_key: None,
            admins: None,
            relays: None,
            nostr_group_id: None,
        };

        let update_result = whitenoise
            .update_group_data(
                &creator_account,
                &group.mls_group_id,
                new_group_data.clone(),
            )
            .await;
        assert!(
            update_result.is_ok(),
            "Failed to update group data: {:?}",
            update_result.unwrap_err()
        );

        // Verify the group data was updated
        let updated_groups = whitenoise.groups(&creator_account, true).await.unwrap();
        let updated_group = updated_groups
            .iter()
            .find(|g| g.mls_group_id == group.mls_group_id)
            .expect("Updated group not found");

        assert_eq!(updated_group.name, new_group_data.name.unwrap());
        assert_eq!(
            updated_group.description,
            new_group_data.description.unwrap()
        );
        assert_eq!(updated_group.image_hash, new_group_data.image_hash.unwrap());
        assert_eq!(
            updated_group.image_key,
            new_group_data.image_key.unwrap().map(Secret::new)
        );
    }

    #[tokio::test]
    async fn test_admin_only_group_functions_reject_non_admin_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 2).await;
        let new_admin_pubkey = members[0].0.pubkey;
        let other_member_pubkey = members[1].0.pubkey;

        let config = create_nostr_group_config_data(vec![creator_account.pubkey]);
        let group = whitenoise
            .create_group(
                &creator_account,
                vec![new_admin_pubkey, other_member_pubkey],
                config,
                None,
            )
            .await
            .unwrap();

        let transfer_admin_rights_update = NostrGroupDataUpdate {
            name: None,
            description: None,
            image_hash: None,
            image_key: None,
            image_nonce: None,
            image_upload_key: None,
            admins: Some(vec![new_admin_pubkey]),
            relays: None,
            nostr_group_id: None,
        };
        whitenoise
            .update_group_data(
                &creator_account,
                &group.mls_group_id,
                transfer_admin_rights_update,
            )
            .await
            .unwrap();

        let new_account = whitenoise.create_identity().await.unwrap();
        let add_members_result = whitenoise
            .add_members_to_group(
                &creator_account,
                &group.mls_group_id,
                vec![new_account.pubkey],
            )
            .await;
        assert!(
            matches!(
                add_members_result,
                Err(WhitenoiseError::AccountNotAuthorized)
            ),
            "Expected AccountNotAuthorized for add_members_to_group, got: {:?}",
            add_members_result
        );

        let remove_members_result = whitenoise
            .remove_members_from_group(
                &creator_account,
                &group.mls_group_id,
                vec![other_member_pubkey],
            )
            .await;
        assert!(
            matches!(
                remove_members_result,
                Err(WhitenoiseError::AccountNotAuthorized)
            ),
            "Expected AccountNotAuthorized for remove_members_from_group, got: {:?}",
            remove_members_result
        );

        let update = NostrGroupDataUpdate {
            name: Some("Updated Name".to_string()),
            description: None,
            image_hash: None,
            image_key: None,
            image_nonce: None,
            image_upload_key: None,
            admins: None,
            relays: None,
            nostr_group_id: None,
        };
        let update_result = whitenoise
            .update_group_data(&creator_account, &group.mls_group_id, update)
            .await;
        assert!(
            matches!(update_result, Err(WhitenoiseError::AccountNotAuthorized)),
            "Expected AccountNotAuthorized for update_group_data, got: {:?}",
            update_result
        );
    }

    #[cfg(test)]
    async fn set_user_relays(
        whitenoise: &Whitenoise,
        user: &User,
        relay_type: RelayType,
        relay_urls: &[&str],
    ) -> Vec<RelayUrl> {
        let existing_relays = user
            .relays(relay_type, &whitenoise.shared.database)
            .await
            .unwrap();
        for relay in existing_relays {
            user.remove_relay(&relay, relay_type, &whitenoise.shared.database)
                .await
                .unwrap();
        }

        let mut configured_urls = Vec::new();
        for url in relay_urls {
            let relay_url = RelayUrl::parse(url).unwrap();
            let relay = whitenoise
                .find_or_create_relay_by_url(&relay_url)
                .await
                .unwrap();
            user.add_relay(&relay, relay_type, &whitenoise.shared.database)
                .await
                .unwrap();
            configured_urls.push(relay_url);
        }

        configured_urls
    }

    #[tokio::test]
    async fn test_resolve_member_delivery_relays_prefers_inbox_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let fallback_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();

        let fallback_user = fallback_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        set_user_relays(
            &whitenoise,
            &fallback_user,
            RelayType::Nip65,
            &["wss://fallback.example.com"],
        )
        .await;

        let member_user = member_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        set_user_relays(
            &whitenoise,
            &member_user,
            RelayType::Nip65,
            &["wss://member-nip65.example.com"],
        )
        .await;
        let inbox_urls = set_user_relays(
            &whitenoise,
            &member_user,
            RelayType::Inbox,
            &[
                "wss://member-inbox-1.example.com",
                "wss://member-inbox-2.example.com",
            ],
        )
        .await;

        let resolved_relays = whitenoise
            .resolve_member_delivery_relays(
                &member_user,
                &fallback_account,
                "tests::resolve_member_delivery_relays_prefers_inbox",
            )
            .await
            .unwrap();

        assert_eq!(Relay::urls(&resolved_relays), inbox_urls);
    }

    #[tokio::test]
    async fn test_resolve_member_delivery_relays_uses_nip65_when_inbox_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let fallback_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();

        let fallback_user = fallback_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        set_user_relays(
            &whitenoise,
            &fallback_user,
            RelayType::Nip65,
            &["wss://fallback.example.com"],
        )
        .await;

        let member_user = member_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        let nip65_urls = set_user_relays(
            &whitenoise,
            &member_user,
            RelayType::Nip65,
            &["wss://member-nip65-only.example.com"],
        )
        .await;
        set_user_relays(&whitenoise, &member_user, RelayType::Inbox, &[]).await;

        let resolved_relays = whitenoise
            .resolve_member_delivery_relays(
                &member_user,
                &fallback_account,
                "tests::resolve_member_delivery_relays_uses_nip65_when_inbox_missing",
            )
            .await
            .unwrap();

        assert_eq!(Relay::urls(&resolved_relays), nip65_urls);
    }

    #[tokio::test]
    async fn test_resolve_member_delivery_relays_falls_back_to_account_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let fallback_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();

        let member_user = member_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        set_user_relays(&whitenoise, &member_user, RelayType::Inbox, &[]).await;
        set_user_relays(&whitenoise, &member_user, RelayType::Nip65, &[]).await;

        let fallback_user = fallback_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        let fallback_urls = set_user_relays(
            &whitenoise,
            &fallback_user,
            RelayType::Nip65,
            &["wss://account-fallback.example.com"],
        )
        .await;

        let resolved_relays = whitenoise
            .resolve_member_delivery_relays(
                &member_user,
                &fallback_account,
                "tests::resolve_member_delivery_relays_falls_back_to_account",
            )
            .await
            .unwrap();

        assert_eq!(Relay::urls(&resolved_relays), fallback_urls);
    }

    #[tokio::test]
    async fn test_resolve_member_delivery_relays_errors_without_any_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let fallback_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();

        let member_user = member_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        set_user_relays(&whitenoise, &member_user, RelayType::Inbox, &[]).await;
        set_user_relays(&whitenoise, &member_user, RelayType::Nip65, &[]).await;

        let fallback_user = fallback_account
            .user(&whitenoise.shared.database)
            .await
            .unwrap();
        set_user_relays(&whitenoise, &fallback_user, RelayType::Nip65, &[]).await;

        let error = whitenoise
            .resolve_member_delivery_relays(
                &member_user,
                &fallback_account,
                "tests::resolve_member_delivery_relays_errors_without_any_relays",
            )
            .await
            .unwrap_err();

        match error {
            WhitenoiseError::MissingWelcomeRelays {
                member_pubkey,
                account_pubkey,
            } => {
                assert_eq!(member_pubkey, member_account.pubkey);
                assert_eq!(account_pubkey, fallback_account.pubkey);
            }
            other => panic!("Expected MissingWelcomeRelays error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_groups_filtering() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Setup accounts
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkeys = vec![members[0].0.pubkey];

        // Create a group
        let admin_pubkeys = vec![creator_account.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys);
        let _group = whitenoise
            .create_group(&creator_account, member_pubkeys, config, None)
            .await
            .unwrap();

        // Test getting all groups
        let all_groups = whitenoise.groups(&creator_account, false).await.unwrap();
        assert!(!all_groups.is_empty(), "Should have at least one group");

        // Test getting only active groups
        let active_groups = whitenoise.groups(&creator_account, true).await.unwrap();
        assert!(
            !active_groups.is_empty(),
            "Should have at least one active group"
        );

        // All groups should be active in this test case
        assert_eq!(
            all_groups.len(),
            active_groups.len(),
            "All groups should be active"
        );

        // All groups should be in a valid state (exact verification depends on state enum implementation)
    }

    /// Helper to create an MDK group directly without auto-accepting the AccountGroup.
    /// This allows tests to manually control the AccountGroup state.
    ///
    /// Uses `whitenoise.create_group()` internally but deletes the auto-created
    /// AccountGroup record so tests can create it fresh with the desired state.
    async fn create_mdk_group_without_auto_accept(
        whitenoise: &Whitenoise,
        account: &Account,
        member_pubkeys: Vec<PublicKey>,
    ) -> group_types::Group {
        // Create group normally (this auto-accepts)
        let config = create_nostr_group_config_data(vec![account.pubkey]);
        let group = whitenoise
            .create_group(account, member_pubkeys, config, None)
            .await
            .unwrap();

        // Delete the auto-created AccountGroup so tests can recreate with desired state
        sqlx::query("DELETE FROM accounts_groups WHERE account_pubkey = ? AND mls_group_id = ?")
            .bind(account.pubkey.to_hex())
            .bind(group.mls_group_id.as_slice())
            .execute(&whitenoise.shared.database.pool)
            .await
            .unwrap();

        group
    }

    #[tokio::test]
    async fn test_visible_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkeys = vec![members[0].0.pubkey];

        // Create 3 MDK groups directly, deleting the auto-accepted AccountGroup
        let group_accepted =
            create_mdk_group_without_auto_accept(&whitenoise, &account, member_pubkeys.clone())
                .await;
        let group_pending =
            create_mdk_group_without_auto_accept(&whitenoise, &account, member_pubkeys.clone())
                .await;
        let group_declined =
            create_mdk_group_without_auto_accept(&whitenoise, &account, member_pubkeys).await;

        // Manually create AccountGroup records with different states:
        // - group_accepted: user_confirmation = Some(true)
        // - group_pending: user_confirmation = None (default from get_or_create)
        // - group_declined: user_confirmation = Some(false)

        let (ag_accepted, _) = whitenoise
            .get_or_create_account_group(&account, &group_accepted.mls_group_id, None)
            .await
            .unwrap();
        ag_accepted.accept(&whitenoise).await.unwrap();

        // Just create the record - stays pending (NULL) by default
        whitenoise
            .get_or_create_account_group(&account, &group_pending.mls_group_id, None)
            .await
            .unwrap();

        let (ag_declined, _) = whitenoise
            .get_or_create_account_group(&account, &group_declined.mls_group_id, None)
            .await
            .unwrap();
        ag_declined.decline(&whitenoise).await.unwrap();

        // Get visible groups - should return accepted + pending, not declined
        let mut visible = whitenoise.visible_groups(&account).await.unwrap();

        assert_eq!(visible.len(), 2);

        // Sort by membership created_at for deterministic ordering
        visible.sort_by_key(|gwm| gwm.membership.created_at);

        // Verify correct groups and their states
        assert_eq!(visible[0].group.mls_group_id, group_accepted.mls_group_id);
        assert!(visible[0].is_accepted());
        assert!(!visible[0].is_pending());

        assert_eq!(visible[1].group.mls_group_id, group_pending.mls_group_id);
        assert!(visible[1].is_pending());
        assert!(!visible[1].is_accepted());
    }

    #[tokio::test]
    async fn test_visible_groups_with_info_includes_group_information() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 2).await;

        // Regular group (non-empty name → GroupType::Group)
        let regular_group = whitenoise
            .create_group(
                &account,
                vec![members[0].0.pubkey],
                create_nostr_group_config_data(vec![account.pubkey]),
                None,
            )
            .await
            .unwrap();

        // DM group (empty name → GroupType::DirectMessage)
        let mut dm_config =
            create_nostr_group_config_data(vec![account.pubkey, members[1].0.pubkey]);
        dm_config.name = "".to_string();
        let dm_group = whitenoise
            .create_group(&account, vec![members[1].0.pubkey], dm_config, None)
            .await
            .unwrap();

        let mut with_info = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .groups()
            .visible_with_info()
            .await
            .unwrap();

        // Both groups are visible; GroupInformation is included for each.
        assert_eq!(with_info.len(), 2, "Both groups should be returned");
        with_info.sort_by_key(|g| g.membership.created_at);

        let regular = with_info
            .iter()
            .find(|g| g.group.mls_group_id == regular_group.mls_group_id)
            .expect("regular group not found");
        assert_eq!(regular.info.group_type, GroupType::Group);

        let dm = with_info
            .iter()
            .find(|g| g.group.mls_group_id == dm_group.mls_group_id)
            .expect("DM group not found");
        assert_eq!(dm.info.group_type, GroupType::DirectMessage);
    }

    #[tokio::test]
    async fn test_visible_groups_with_info_excludes_declined() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 2).await;

        let group_accepted =
            create_mdk_group_without_auto_accept(&whitenoise, &account, vec![members[0].0.pubkey])
                .await;
        let group_declined =
            create_mdk_group_without_auto_accept(&whitenoise, &account, vec![members[1].0.pubkey])
                .await;

        let (ag_accepted, _) = whitenoise
            .get_or_create_account_group(&account, &group_accepted.mls_group_id, None)
            .await
            .unwrap();
        ag_accepted.accept(&whitenoise).await.unwrap();

        let (ag_declined, _) = whitenoise
            .get_or_create_account_group(&account, &group_declined.mls_group_id, None)
            .await
            .unwrap();
        ag_declined.decline(&whitenoise).await.unwrap();

        let with_info = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .groups()
            .visible_with_info()
            .await
            .unwrap();

        assert_eq!(with_info.len(), 1, "Declined group should be excluded");
        assert_eq!(with_info[0].group.mls_group_id, group_accepted.mls_group_id);
        assert_eq!(with_info[0].info.group_type, GroupType::Group);
    }

    #[tokio::test]
    async fn test_visible_groups_with_info_caller_can_filter_dms() {
        // Demonstrates the intended usage: caller filters on group_type.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 2).await;

        // One regular group, one DM
        let regular_group = whitenoise
            .create_group(
                &account,
                vec![members[0].0.pubkey],
                create_nostr_group_config_data(vec![account.pubkey]),
                None,
            )
            .await
            .unwrap();

        let mut dm_config =
            create_nostr_group_config_data(vec![account.pubkey, members[1].0.pubkey]);
        dm_config.name = "".to_string();
        let _dm = whitenoise
            .create_group(&account, vec![members[1].0.pubkey], dm_config, None)
            .await
            .unwrap();

        let non_dms: Vec<_> = whitenoise
            .session(&account.pubkey)
            .unwrap()
            .groups()
            .visible_with_info()
            .await
            .unwrap()
            .into_iter()
            .filter(|g| g.info.group_type == GroupType::Group)
            .collect();

        assert_eq!(non_dms.len(), 1);
        assert_eq!(non_dms[0].group.mls_group_id, regular_group.mls_group_id);
    }

    #[tokio::test]
    async fn test_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Setup creator account
        let creator_account = whitenoise.create_identity().await.unwrap();

        // Setup member accounts
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkeys = vec![members[0].0.pubkey];

        // Create a group
        let admin_pubkeys = vec![creator_account.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys.clone());
        let created_group = whitenoise
            .create_group(
                &creator_account,
                member_pubkeys.clone(),
                config.clone(),
                None,
            )
            .await
            .unwrap();

        // Test: Successfully retrieve the created group
        let retrieved_group = whitenoise
            .group(&creator_account, &created_group.mls_group_id)
            .await;

        assert!(
            retrieved_group.is_ok(),
            "Failed to retrieve group: {:?}",
            retrieved_group.unwrap_err()
        );

        let retrieved_group = retrieved_group.unwrap();
        assert_eq!(retrieved_group.mls_group_id, created_group.mls_group_id);
        assert_eq!(retrieved_group.name, config.name);
        assert_eq!(retrieved_group.description, config.description);
        assert_eq!(retrieved_group.admin_pubkeys, created_group.admin_pubkeys);

        // Test: Attempt to retrieve non-existent group
        let fake_group_id = GroupId::from_slice(&[255u8; 32]);
        let result = whitenoise.group(&creator_account, &fake_group_id).await;

        assert!(result.is_err(), "Expected error for non-existent group");
        match result.unwrap_err() {
            WhitenoiseError::GroupNotFound => {
                // Expected error type
            }
            other => panic!("Expected GroupNotFound error, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_leave_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Setup creator and members
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 2).await;
        let member_accounts = members.iter().map(|(acc, _)| acc).collect::<Vec<_>>();
        let member_pubkeys = member_accounts
            .iter()
            .map(|acc| acc.pubkey)
            .collect::<Vec<_>>();

        // Create group with creator and members as admins (so they can process the leave proposal)
        let admin_pubkeys = vec![creator_account.pubkey, member_pubkeys[0]];
        let config = create_nostr_group_config_data(admin_pubkeys);
        let group = whitenoise
            .create_group(&creator_account, member_pubkeys.clone(), config, None)
            .await
            .unwrap();

        // Verify initial membership
        let initial_members = whitenoise
            .group_members(&creator_account, &group.mls_group_id)
            .await
            .unwrap();
        assert_eq!(initial_members.len(), 3); // creator + 2 members

        // Creator must self-demote before leaving (MIP-03: admins cannot SelfRemove)
        whitenoise
            .self_demote(&creator_account, &group.mls_group_id)
            .await
            .expect("self_demote should succeed (another admin exists)");

        // Creator leaves the group (creates SelfRemove proposal)
        // Note: In a real scenario, members would need to accept welcome messages
        // to have access to the group. For this test, we use the creator who
        // has immediate access to the group.
        let leave_result = whitenoise
            .leave_group(&creator_account, &group.mls_group_id)
            .await;

        assert!(
            leave_result.is_ok(),
            "Failed to initiate leave group: {:?}",
            leave_result.unwrap_err()
        );

        // Note: At this point, the member has only created a proposal to leave.
        // The actual removal would happen when an admin processes the commit,
        // but that's part of the message processing pipeline that would be
        // tested separately in integration tests.

        // A second leave attempt must be rejected — the account already departed
        let second_leave = whitenoise
            .leave_group(&creator_account, &group.mls_group_id)
            .await;
        assert!(
            matches!(second_leave, Err(WhitenoiseError::AlreadyDepartedFromGroup)),
            "Expected AlreadyDepartedFromGroup, got: {:?}",
            second_leave
        );
    }

    // TODO(phase-16): Re-enable once storage moves into AccountSession.
    // MediaOps::upload_group_image calls Self::wn() for media_files().store_and_record(),
    // which requires the Whitenoise singleton unavailable in unit tests.
    #[ignore]
    #[allow(deprecated)]
    #[tokio::test]
    async fn test_upload_group_image() {
        use tempfile::NamedTempFile;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let mut blossom_server = mockito::Server::new_async().await;
        let blossom_url = mock_blossom_url(&blossom_server);
        let blossom_url_for_response = blossom_url.clone();
        let _upload_mock = blossom_server
            .mock("PUT", "/upload")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let descriptor = mock_blob_descriptor(
                    &blossom_url_for_response,
                    request.body().unwrap(),
                    "image/png",
                );
                serde_json::to_vec(&descriptor).unwrap()
            })
            .create_async()
            .await;

        // Setup creator and member
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkeys = vec![members[0].0.pubkey];

        // Create group with creator as admin
        let admin_pubkeys = vec![creator_account.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys);
        let group = whitenoise
            .create_group(&creator_account, member_pubkeys, config, None)
            .await
            .unwrap();

        // Create a valid 100x100 PNG image using the image crate
        // (must be large enough for blurhash generation)
        let img = ::image::RgbaImage::from_pixel(100, 100, ::image::Rgba([255u8, 0, 0, 255]));
        let temp_file = NamedTempFile::new().unwrap();
        img.save_with_format(temp_file.path(), ::image::ImageFormat::Png)
            .unwrap();
        let temp_path = temp_file.path().to_str().unwrap();

        // Read the original image data for later comparison
        let test_image_data = tokio::fs::read(temp_path).await.unwrap();

        // Use test options to skip blurhash generation (which has issues with small test images)
        let test_options = MediaProcessingOptions {
            generate_blurhash: false,
            ..Default::default()
        };
        let result = whitenoise
            .upload_group_image(
                &creator_account,
                &group.mls_group_id,
                temp_path,
                Some(blossom_url),
                Some(test_options),
            )
            .await;

        assert!(
            result.is_ok(),
            "Failed to upload group image: {:?}",
            result.unwrap_err()
        );

        let (hash, key, nonce) = result.unwrap();

        // Verify the returned values are valid
        assert_ne!(hash, [0u8; 32], "Hash should not be all zeros");
        assert_ne!(key, [0u8; 32], "Key should not be all zeros");
        assert_ne!(nonce, [0u8; 12], "Nonce should not be all zeros");

        // Update the group with the new image metadata
        let update = NostrGroupDataUpdate {
            name: None,
            description: None,
            image_hash: Some(Some(hash)),
            image_key: Some(Some(key)),
            image_nonce: Some(Some(nonce)),
            image_upload_key: None,
            admins: None,
            relays: None,
            nostr_group_id: None,
        };

        let update_result = whitenoise
            .update_group_data(&creator_account, &group.mls_group_id, update)
            .await;

        assert!(
            update_result.is_ok(),
            "Failed to update group data: {:?}",
            update_result.unwrap_err()
        );

        // Verify the group data was updated
        let updated_groups = whitenoise.groups(&creator_account, true).await.unwrap();
        let updated_group = updated_groups
            .iter()
            .find(|g| g.mls_group_id == group.mls_group_id)
            .expect("Updated group not found");

        assert_eq!(updated_group.image_hash, Some(hash));
        assert_eq!(updated_group.image_key, Some(Secret::new(key)));
        assert_eq!(updated_group.image_nonce, Some(Secret::new(nonce)));

        // Verify the image was cached immediately after upload by retrieving it
        // (should be instant since it's cached)
        let cached_path = whitenoise
            .get_group_image_path(&creator_account, &group.mls_group_id)
            .await
            .unwrap();

        assert!(
            cached_path.is_some(),
            "Uploaded image should be cached and retrievable"
        );

        let cached_path = cached_path.unwrap();
        assert!(
            cached_path.exists(),
            "Cached image file should exist at: {}",
            cached_path.display()
        );

        // Verify the cached content matches the original
        let cached_content = tokio::fs::read(&cached_path).await.unwrap();
        assert_eq!(
            cached_content, test_image_data,
            "Cached image content should match original"
        );

        // Verify the nostr_key (upload keypair) was stored in the database
        let media_file = crate::whitenoise::database::media_files::MediaFile::find_by_hash(
            &whitenoise.shared.database,
            &hash,
        )
        .await
        .unwrap();
        assert!(media_file.is_some(), "Media file should be in database");
        assert!(
            media_file.unwrap().nostr_key.is_some(),
            "Nostr key should be stored for group images for cleanup"
        );
    }

    // TODO(phase-16): Re-enable once storage moves into AccountSession.
    // MediaOps::check_cached_image calls Self::wn() for media_files().find_file_with_prefix(),
    // which requires the Whitenoise singleton unavailable in unit tests.
    #[ignore]
    #[allow(deprecated)]
    #[tokio::test]
    async fn test_sync_group_image_cache() {
        use std::time::Duration;
        use tempfile::NamedTempFile;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let mut blossom_server = mockito::Server::new_async().await;
        let blossom_url = mock_blossom_url(&blossom_server);
        let blossom_url_for_response = blossom_url.clone();
        let _upload_mock = blossom_server
            .mock("PUT", "/upload")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let descriptor = mock_blob_descriptor(
                    &blossom_url_for_response,
                    request.body().unwrap(),
                    "image/jpeg",
                );
                serde_json::to_vec(&descriptor).unwrap()
            })
            .create_async()
            .await;

        // Setup creator and member accounts
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = &members[0].0;
        let member_pubkeys = vec![member_account.pubkey];

        // Create group with creator as admin
        let admin_pubkeys = vec![creator_account.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys);
        let group = whitenoise
            .create_group(&creator_account, member_pubkeys, config, None)
            .await
            .unwrap();

        // Create a valid 100x100 JPEG image using the image crate
        // (must be large enough for blurhash generation)
        // JPEG does not support an alpha channel, so use RgbImage (Rgb8) rather than RgbaImage.
        let img = ::image::RgbImage::from_pixel(100, 100, ::image::Rgb([255u8, 0, 0]));
        let temp_file = NamedTempFile::new().unwrap();
        img.save_with_format(temp_file.path(), ::image::ImageFormat::Jpeg)
            .unwrap();
        let temp_path = temp_file.path().to_str().unwrap();

        // Read the original image data for later comparison
        let test_image_data = tokio::fs::read(temp_path).await.unwrap();

        // Use test options to skip blurhash generation (which has issues with small test images)
        let test_options = MediaProcessingOptions {
            generate_blurhash: false,
            ..Default::default()
        };
        let (hash, key, nonce) = whitenoise
            .upload_group_image(
                &creator_account,
                &group.mls_group_id,
                temp_path,
                Some(blossom_url),
                Some(test_options),
            )
            .await
            .unwrap();

        // Update the group data with the image metadata
        let update = NostrGroupDataUpdate {
            name: None,
            description: None,
            image_hash: Some(Some(hash)),
            image_key: Some(Some(key)),
            image_nonce: Some(Some(nonce)),
            image_upload_key: None,
            admins: None,
            relays: None,
            nostr_group_id: None,
        };

        whitenoise
            .update_group_data(&creator_account, &group.mls_group_id, update)
            .await
            .unwrap();

        // Give time for the commit to propagate
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify the creator can retrieve the cached image
        let cached_path_opt = whitenoise
            .get_group_image_path(&creator_account, &group.mls_group_id)
            .await
            .unwrap();

        assert!(
            cached_path_opt.is_some(),
            "Creator should have cached image path"
        );

        let cached_path = cached_path_opt.unwrap();
        assert!(
            cached_path.exists(),
            "Cached image should exist at: {}",
            cached_path.display()
        );

        // Verify the cached content matches the original
        let cached_content = tokio::fs::read(&cached_path).await.unwrap();
        assert_eq!(
            cached_content, test_image_data,
            "Cached image content should match original"
        );

        // Verify subsequent access returns the same cached path (instant)
        let cached_again = whitenoise
            .get_group_image_path(&creator_account, &group.mls_group_id)
            .await
            .unwrap();

        assert!(cached_again.is_some());
        assert_eq!(
            cached_again.unwrap(),
            cached_path,
            "Second retrieval should return same cached path"
        );
    }

    // TODO(phase-16): Re-enable once storage moves into AccountSession.
    // MediaOps::upload_chat_media calls Self::wn() for media_files().store_and_record(),
    // which requires the Whitenoise singleton unavailable in unit tests.
    #[ignore]
    #[allow(deprecated)]
    #[tokio::test]
    async fn test_upload_chat_media() {
        use tempfile::NamedTempFile;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let mut blossom_server = mockito::Server::new_async().await;
        let blossom_url = mock_blossom_url(&blossom_server);
        let blossom_url_for_response = blossom_url.clone();
        let _upload_mock = blossom_server
            .mock("PUT", "/upload")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let descriptor = mock_blob_descriptor(
                    &blossom_url_for_response,
                    request.body().unwrap(),
                    "image/png",
                );
                serde_json::to_vec(&descriptor).unwrap()
            })
            .create_async()
            .await;

        // Setup creator and member
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = &members[0].0;
        let member_pubkeys = vec![member_account.pubkey];

        // Create group
        let admin_pubkeys = vec![creator_account.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys);
        let group = whitenoise
            .create_group(&creator_account, member_pubkeys, config, None)
            .await
            .unwrap();

        // Create a valid 100x100 PNG image
        let img = ::image::RgbaImage::from_pixel(100, 100, ::image::Rgba([0u8, 255, 0, 255]));
        let temp_file = NamedTempFile::new().unwrap();
        img.save_with_format(temp_file.path(), ::image::ImageFormat::Png)
            .unwrap();
        let temp_path = temp_file.path().to_str().unwrap();

        // Test upload with creator account
        // Note: In a real scenario, members would upload after processing their welcome message,
        // which gives them access to the group secrets needed for encryption key derivation.
        // For this unit test, we use the creator who has immediate group access.
        let test_options = MediaProcessingOptions {
            generate_blurhash: false,
            ..Default::default()
        };
        let result = whitenoise
            .upload_chat_media(
                &creator_account,
                &group.mls_group_id,
                temp_path,
                Some(blossom_url),
                Some(test_options),
            )
            .await;

        assert!(
            result.is_ok(),
            "Failed to upload chat image as non-admin: {:?}",
            result.unwrap_err()
        );

        let media_file = result.unwrap();

        // Verify the media file contains valid data
        assert_ne!(
            media_file.encrypted_file_hash,
            vec![0u8; 32],
            "Encrypted hash should not be all zeros"
        );
        assert!(media_file.blossom_url.is_some(), "URL should be present");
        assert!(
            media_file.nostr_key.is_some(),
            "Nostr key should be stored for chat images"
        );
        assert_eq!(media_file.mime_type, "image/png");
        assert_eq!(media_file.media_type, "chat_media");
        assert!(
            media_file.file_path.exists(),
            "Cached file should exist at: {}",
            media_file.file_path.display()
        );

        // Verify the original filename was stored in metadata
        assert!(media_file.file_metadata.is_some());
        let metadata = media_file.file_metadata.as_ref().unwrap();
        assert!(
            metadata.original_filename.is_some(),
            "Original filename should be stored"
        );
    }

    /// MP4 ftyp header only (matches `types::tests::test_detect_non_image_mp4_video`).
    fn minimal_mp4_fixture() -> Vec<u8> {
        vec![
            0x00, 0x00, 0x00, 0x18, // Box size
            b'f', b't', b'y', b'p', // "ftyp"
            b'i', b's', b'o', b'm', // Brand: isom
            0x00, 0x00, 0x00, 0x00, // Version
            b'i', b's', b'o', b'm', // Compatible brands
            b'm', b'p', b'4', b'2',
        ]
    }

    #[tokio::test]
    async fn test_upload_chat_media_video_stores_original_filename_in_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let mut blossom_server = mockito::Server::new_async().await;
        let blossom_url = mock_blossom_url(&blossom_server);
        let blossom_url_for_response = blossom_url.clone();
        let _upload_mock = blossom_server
            .mock("PUT", "/upload")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let descriptor = mock_blob_descriptor(
                    &blossom_url_for_response,
                    request.body().unwrap(),
                    "video/mp4",
                );
                serde_json::to_vec(&descriptor).unwrap()
            })
            .create_async()
            .await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = &members[0].0;
        let member_pubkeys = vec![member_account.pubkey];

        let admin_pubkeys = vec![creator_account.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys);
        let group = whitenoise
            .create_group(&creator_account, member_pubkeys, config, None)
            .await
            .unwrap();

        let temp_dir = tempfile::tempdir().unwrap();
        let expected_basename = "video_picker_ABC123.mp4";
        let video_path = temp_dir.path().join(expected_basename);
        tokio::fs::write(&video_path, minimal_mp4_fixture())
            .await
            .unwrap();
        let temp_path = video_path.to_str().unwrap();

        let test_options = MediaProcessingOptions {
            generate_blurhash: false,
            ..Default::default()
        };
        let result = whitenoise
            .upload_chat_media(
                &creator_account,
                &group.mls_group_id,
                temp_path,
                Some(blossom_url),
                Some(test_options),
            )
            .await;

        assert!(
            result.is_ok(),
            "Failed to upload chat video: {:?}",
            result.unwrap_err()
        );

        let media_file = result.unwrap();
        assert_eq!(media_file.mime_type, "video/mp4");
        assert_eq!(media_file.media_type, "chat_media");

        let metadata = media_file
            .file_metadata
            .as_ref()
            .expect("video uploads must persist FileMetadata for MIP-04 decryption");
        assert_eq!(
            metadata.original_filename.as_deref(),
            Some(expected_basename),
            "receiver imeta / decrypt must use the same basename the sender encrypted with"
        );
        assert!(
            metadata.dimensions.is_none(),
            "minimal MP4 fixture has no parsed dimensions unless MDK adds video support"
        );

        let original_hash_bytes: [u8; 32] = media_file
            .original_file_hash
            .as_ref()
            .expect("chat_media should store original_file_hash")
            .as_slice()
            .try_into()
            .expect("original_file_hash must be 32 bytes");

        let reloaded = MediaFile::find_by_original_hash_and_group(
            &whitenoise.shared.database,
            &original_hash_bytes,
            &group.mls_group_id,
            &creator_account.pubkey,
        )
        .await
        .unwrap()
        .expect("MediaFile row should exist after upload");

        assert_eq!(
            reloaded
                .file_metadata
                .as_ref()
                .and_then(|m| m.original_filename.as_deref()),
            Some(expected_basename),
            "filename metadata must round-trip through SQLite for download/decrypt"
        );
    }

    // ── publish_event_with_retry tests ──────────────────────────────────

    #[tokio::test]
    async fn test_publish_event_with_retry_succeeds() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Use a real account so event tracking succeeds after relay acceptance
        let account = whitenoise.create_identity().await.unwrap();
        let signer = whitenoise.get_signer_for_account(&account).unwrap();
        let event = EventBuilder::text_note("retry-test-success")
            .sign(&signer)
            .await
            .unwrap();
        let relay_urls = vec![RelayUrl::parse("ws://localhost:8080").unwrap()];

        let result = whitenoise
            .publish_event_with_retry(event, &account.pubkey, &relay_urls)
            .await;
        assert!(
            result.is_ok(),
            "publish_event_with_retry should succeed against a reachable relay: {:?}",
            result.unwrap_err()
        );
    }

    #[tokio::test]
    async fn test_publish_event_with_retry_fails_after_retries() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys = Keys::generate();
        let event = EventBuilder::text_note("retry-test-failure")
            .sign_with_keys(&keys)
            .unwrap();
        // Use loopback IPs so connection refusal is instant (no DNS lookup).
        let unreachable = vec![
            RelayUrl::parse("ws://127.0.0.1:1").unwrap(),
            RelayUrl::parse("ws://127.0.0.1:2").unwrap(),
        ];

        // Pause time so exponential backoff sleeps complete without burning
        // real seconds.  The whitenoise + relay setup above ran with real time.
        tokio::time::pause();
        let result = whitenoise
            .publish_event_with_retry(event, &keys.public_key(), &unreachable)
            .await;
        tokio::time::resume();
        assert!(
            result.is_err(),
            "publish_event_with_retry should fail when no relay accepts the event"
        );
    }

    // ── Ordering tests: publish failure must not advance local state ─────
    //
    // These tests call the actual production methods (add_members_to_group,
    // remove_members_from_group, update_group_data) against groups whose
    // relays are configured to unreachable ports. This ensures the tests
    // exercise the real publish-then-merge ordering and would catch
    // regressions if someone reorders the code in the future.

    /// Unreachable relay URLs used to force publish failures in ordering tests.
    const UNREACHABLE_RELAYS: &[&str] = &["ws://localhost:1", "ws://localhost:2"];

    /// Creates a group whose MLS-stored relays point to unreachable ports.
    ///
    /// The group is created with real Docker relays (so welcome fan-out
    /// succeeds during `create_group`), then a successful `update_group_data`
    /// swaps the relays to unreachable URLs. After this, any subsequent
    /// call to `ensure_group_relays` returns the unreachable relays.
    async fn create_group_with_unreachable_relays(
        whitenoise: &Whitenoise,
    ) -> (group_types::Group, Account, Vec<(Account, Keys)>) {
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(whitenoise, 2).await;
        let member_pks = members.iter().map(|(a, _)| a.pubkey).collect::<Vec<_>>();

        // Create with real relays so welcome messages succeed
        let config = create_nostr_group_config_data(vec![creator.pubkey]);
        let group = whitenoise
            .create_group(&creator, member_pks, config, None)
            .await
            .unwrap();

        // Now swap the group's relays to unreachable ports via update_group_data
        let unreachable_urls: Vec<RelayUrl> = UNREACHABLE_RELAYS
            .iter()
            .map(|u| RelayUrl::parse(u).unwrap())
            .collect();
        let relay_swap = NostrGroupDataUpdate {
            name: None,
            description: None,
            image_hash: None,
            image_key: None,
            image_nonce: None,
            image_upload_key: None,
            admins: None,
            relays: Some(unreachable_urls),
            nostr_group_id: None,
        };
        whitenoise
            .update_group_data(&creator, &group.mls_group_id, relay_swap)
            .await
            .unwrap();

        (group, creator, members)
    }

    #[tokio::test]
    async fn test_add_members_no_merge_on_publish_failure() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (group, creator, _existing) = create_group_with_unreachable_relays(&whitenoise).await;
        let group_id = &group.mls_group_id;

        let members_before = whitenoise.group_members(&creator, group_id).await.unwrap();

        // Prepare a new member with a key package on the real relay
        let new_members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let new_pk = new_members[0].0.pubkey;

        // Pause time so backoff sleeps complete instantly; resume before DB reads.
        tokio::time::pause();
        // Call the actual production method — it will fail at publish
        // because the group's relays are now unreachable.
        let result = whitenoise
            .add_members_to_group(&creator, group_id, vec![new_pk])
            .await;
        tokio::time::resume();
        assert!(result.is_err(), "Should fail when relays are unreachable");

        // Verify: group membership is unchanged (merge did not happen)
        let members_after = whitenoise.group_members(&creator, group_id).await.unwrap();
        assert_eq!(
            members_before.len(),
            members_after.len(),
            "Member count should be unchanged when publish fails \
             (pending commit must not be merged)"
        );
        assert!(
            !members_after.contains(&new_pk),
            "New member should not appear in the group"
        );
    }

    #[tokio::test]
    async fn test_remove_members_no_merge_on_publish_failure() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (group, creator, existing) = create_group_with_unreachable_relays(&whitenoise).await;
        let group_id = &group.mls_group_id;
        let member_to_remove = existing[0].0.pubkey;

        let members_before = whitenoise.group_members(&creator, group_id).await.unwrap();
        assert!(members_before.contains(&member_to_remove));

        // Pause time so backoff sleeps complete instantly; resume before DB reads.
        tokio::time::pause();
        // Call the actual production method — fails at publish
        let result = whitenoise
            .remove_members_from_group(&creator, group_id, vec![member_to_remove])
            .await;
        tokio::time::resume();
        assert!(result.is_err(), "Should fail when relays are unreachable");

        // Verify: member is still in the group (merge did not happen)
        let members_after = whitenoise.group_members(&creator, group_id).await.unwrap();
        assert_eq!(
            members_before.len(),
            members_after.len(),
            "Member count should be unchanged when publish fails"
        );
        assert!(
            members_after.contains(&member_to_remove),
            "Removed member should still be present (merge must not have happened)"
        );
    }

    #[tokio::test]
    async fn test_update_group_data_no_merge_on_publish_failure() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (group, creator, _existing) = create_group_with_unreachable_relays(&whitenoise).await;
        let group_id = &group.mls_group_id;

        let group_before = whitenoise.group(&creator, group_id).await.unwrap();

        // Pause time so backoff sleeps complete instantly; resume before DB reads.
        tokio::time::pause();
        // Call the actual production method — fails at publish
        let new_data = NostrGroupDataUpdate {
            name: Some("Should Not Appear".to_string()),
            description: Some("This update should not be applied".to_string()),
            image_hash: None,
            image_key: None,
            image_nonce: None,
            image_upload_key: None,
            admins: None,
            relays: None,
            nostr_group_id: None,
        };
        let result = whitenoise
            .update_group_data(&creator, group_id, new_data)
            .await;
        tokio::time::resume();
        assert!(result.is_err(), "Should fail when relays are unreachable");

        // Verify: group data is unchanged (merge did not happen)
        let group_after = whitenoise.group(&creator, group_id).await.unwrap();
        assert_eq!(
            group_before.name, group_after.name,
            "Group name should be unchanged when publish fails"
        );
        assert_eq!(
            group_before.description, group_after.description,
            "Group description should be unchanged when publish fails"
        );
    }

    #[tokio::test]
    async fn group_required_proposals_returns_self_remove_for_native_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();

        let config = create_nostr_group_config_data(vec![creator_account.pubkey]);
        let group = whitenoise
            .create_group(&creator_account, vec![member_account.pubkey], config, None)
            .await
            .unwrap();

        let required = whitenoise
            .group_required_proposals(&creator_account, &group.mls_group_id)
            .await
            .unwrap();

        assert_eq!(required, BTreeSet::from([RequiredProposal::SelfRemove]));
    }

    #[tokio::test]
    async fn group_required_proposals_errors_on_missing_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let missing_group_id = GroupId::from_slice(&[0; 32]);

        let err = whitenoise
            .group_required_proposals(&account, &missing_group_id)
            .await
            .expect_err("fabricated group ID must error");

        assert!(matches!(err, WhitenoiseError::GroupNotFound));
    }
}

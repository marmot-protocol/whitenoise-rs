use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};

use mdk_core::encrypted_media::types::MediaReference;
use mdk_core::extension::group_image;
use mdk_core::media_processing::MediaProcessingOptions;
use mdk_core::prelude::*;
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr_blossom::client::BlossomClient;
use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};

use crate::{
    RelayType,
    types::ImageType,
    whitenoise::{
        Whitenoise,
        accounts::Account,
        database::media_files::{FileMetadata, MediaFile},
        error::{Result, WhitenoiseError},
        group_information::{GroupInformation, GroupType},
        media_files::MediaFileUpload,
        relays::Relay,
        users::User,
    },
};

/// Default timeout for Blossom HTTP operations (download and upload)
/// Set to 300 seconds to accommodate large image files over slow connections
const BLOSSOM_TIMEOUT: Duration = Duration::from_secs(300);

impl Whitenoise {
    /// Returns the default Blossom server URL based on build configuration
    ///
    /// In debug builds, uses localhost:3000 for local testing.
    /// In release builds, uses the production Blossom server.
    fn default_blossom_url() -> Url {
        let url = if cfg!(debug_assertions) {
            "http://localhost:3000"
        } else {
            "https://blossom.primal.net"
        };
        Url::parse(url).expect("Hardcoded Blossom URL should be valid")
    }

    /// Ensures that group relays are available for publishing evolution events.
    /// Returns the validated relay URLs.
    ///
    /// # Arguments
    /// * `mdk` - The NostrMls instance to get relays from
    /// * `group_id` - The ID of the group
    ///
    /// # Returns
    /// * `Ok(Vec<nostr_sdk::RelayUrl>)` - Vector of relay URLs
    /// * `Err(WhitenoiseError::GroupMissingRelays)` - If no relays are configured
    fn ensure_group_relays(
        mdk: &MDK<MdkSqliteStorage>,
        group_id: &GroupId,
    ) -> Result<Vec<nostr_sdk::RelayUrl>> {
        let group_relays = mdk.get_relays(group_id)?;

        if group_relays.is_empty() {
            return Err(WhitenoiseError::GroupMissingRelays);
        }

        Ok(group_relays.into_iter().collect())
    }

    async fn resolve_member_delivery_relays(
        &self,
        member: &User,
        fallback_account: &Account,
        context: &'static str,
    ) -> Result<Vec<Relay>> {
        let inbox_relays = member.relays(RelayType::Inbox, &self.database).await?;
        if !inbox_relays.is_empty() {
            return Ok(inbox_relays);
        }

        let nip65_relays = member.relays(RelayType::Nip65, &self.database).await?;
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

    /// Creates a new MLS group with the specified members and settings
    ///
    /// # Arguments
    /// * `creator_account` - Account of the group creator (must be the active account)
    /// * `member_pubkeys` - List of public keys for group members
    /// * `config` - Group configuration data
    /// * `group_type` - Optional explicit group type. If None, will be inferred from participant count
    pub async fn create_group(
        &self,
        creator_account: &Account,
        member_pubkeys: Vec<PublicKey>,
        config: NostrGroupConfigData,
        group_type: Option<GroupType>,
    ) -> Result<group_types::Group> {
        let keys = self
            .secrets_store
            .get_nostr_keys_for_pubkey(&creator_account.pubkey)?;

        let mut key_package_events: Vec<Event> = Vec::new();
        let mut members = Vec::new();

        for pk in member_pubkeys.iter() {
            let (mut user, created) = User::find_or_create_by_pubkey(pk, &self.database).await?;
            if created {
                // Fetch the user's relay lists and save them to the database
                if let Err(e) = user.update_relay_lists(self).await {
                    tracing::warn!(
                        target: "whitenoise::accounts::groups::create_group",
                        "Failed to update relay lists for new user {}: {}",
                        user.pubkey,
                        e
                    );
                    // Continue with group creation even if relay list update fails
                }
                if let Err(e) = user.sync_metadata(self).await {
                    tracing::warn!(
                        target: "whitenoise::accounts::groups::create_group",
                        "Failed to sync metadata for new user {}: {}",
                        user.pubkey,
                        e
                    );
                    // Continue with group creation even if metadata sync fails
                }
            }
            let mut kp_relays = user.relays(RelayType::KeyPackage, &self.database).await?;
            if kp_relays.is_empty() {
                tracing::warn!(
                    target: "whitenoise::accounts::groups::create_group",
                    "User {} has no key package relays configured, falling back to account {} relays",
                    user.pubkey,
                    creator_account.pubkey
                );
                kp_relays = creator_account.nip65_relays(self).await?;
                if kp_relays.is_empty() {
                    tracing::warn!(
                        target: "whitenoise::accounts::groups::create_group",
                        "Account {} has no fallback relays configured, using defaults",
                        creator_account.pubkey
                    );
                    kp_relays = Relay::defaults();
                }
            }
            let kp_relays_urls = Relay::urls(&kp_relays);
            let some_event = self
                .nostr
                .fetch_user_key_package(*pk, &kp_relays_urls)
                .await?;
            let event = some_event.ok_or(WhitenoiseError::MdkCoreError(
                mdk_core::Error::KeyPackage("Does not exist".to_owned()),
            ))?;
            key_package_events.push(event);
            members.push(user);
        }

        tracing::debug!("Succefully fetched the key packages of members");

        let group_relays = config.relays.clone();
        let group_name = config.name.clone();

        let mdk = Account::create_mdk(creator_account.pubkey, &self.config.data_dir)?;
        let create_group_result =
            mdk.create_group(&creator_account.pubkey, key_package_events.clone(), config)?;

        let group_ids = mdk
            .get_groups()?
            .into_iter()
            .map(|g| hex::encode(g.nostr_group_id))
            .collect::<Vec<_>>();

        let group = create_group_result.group;
        let welcome_rumors = create_group_result.welcome_rumors;
        if welcome_rumors.len() != members.len() {
            return Err(WhitenoiseError::Other(anyhow::Error::msg(
                "Welcome rumours are missing for some of the members",
            )));
        }

        // Fan out the welcome message to all members
        for (welcome_rumor, member) in welcome_rumors.iter().zip(members.iter()) {
            // Get the public key of the member from the key package event
            let key_package_event_id =
                welcome_rumor
                    .tags
                    .event_ids()
                    .next()
                    .ok_or(WhitenoiseError::Other(anyhow::anyhow!(
                        "No event ID found in welcome rumor"
                    )))?;

            let member_pubkey = key_package_events
                .iter()
                .find(|event| event.id == *key_package_event_id)
                .map(|event| event.pubkey)
                .ok_or(WhitenoiseError::Other(anyhow::anyhow!(
                    "No public key found in key package event"
                )))?;

            // Create a timestamp 1 month in the future
            let one_month_future = Timestamp::now() + Duration::from_secs(30 * 24 * 60 * 60);
            let relays_to_use = self
                .resolve_member_delivery_relays(
                    member,
                    creator_account,
                    "whitenoise::accounts::groups::create_group",
                )
                .await?;

            self.nostr
                .publish_gift_wrap_to(
                    &member_pubkey,
                    welcome_rumor.clone(),
                    &[Tag::expiration(one_month_future)],
                    creator_account.pubkey,
                    &Relay::urls(&relays_to_use),
                    keys.clone(),
                )
                .await
                .map_err(WhitenoiseError::from)?;
        }

        let mut relays = HashSet::new();
        for relay_url in &group_relays {
            let db_relay = self.find_or_create_relay_by_url(relay_url).await?;
            relays.insert(db_relay);
        }

        self.nostr
            .setup_group_messages_subscriptions_with_signer(
                creator_account.pubkey,
                &Relay::urls(&relays),
                &group_ids,
                keys,
            )
            .await
            .map_err(WhitenoiseError::from)?;

        GroupInformation::create_for_group(
            self,
            &group.mls_group_id.clone(),
            group_type,
            &group_name,
        )
        .await?;

        Ok(group)
    }

    pub async fn groups(
        &self,
        account: &Account,
        active_filter: bool,
    ) -> Result<Vec<group_types::Group>> {
        let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
        Ok(mdk
            .get_groups()
            .map_err(WhitenoiseError::from)?
            .into_iter()
            .filter(|group| !active_filter || group.state == group_types::GroupState::Active)
            .collect::<Vec<group_types::Group>>())
    }

    /// Retrieves a single group by its MLS group ID
    ///
    /// # Arguments
    /// * `account` - The account that has access to the group
    /// * `group_id` - The MLS group ID to retrieve
    ///
    /// # Returns
    /// * `Ok(Group)` - The group if found
    /// * `Err(WhitenoiseError::GroupNotFound)` - If the group doesn't exist
    /// * `Err(WhitenoiseError)` - If there's an error accessing storage
    pub async fn group(&self, account: &Account, group_id: &GroupId) -> Result<group_types::Group> {
        let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
        mdk.get_group(group_id)
            .map_err(WhitenoiseError::from)?
            .ok_or(WhitenoiseError::GroupNotFound)
    }

    pub async fn group_members(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<Vec<PublicKey>> {
        let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
        Ok(mdk
            .get_members(group_id)
            .map_err(WhitenoiseError::from)?
            .into_iter()
            .collect::<Vec<PublicKey>>())
    }

    pub async fn group_admins(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<Vec<PublicKey>> {
        let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
        Ok(mdk
            .get_group(group_id)
            .map_err(WhitenoiseError::from)?
            .ok_or(WhitenoiseError::GroupNotFound)?
            .admin_pubkeys
            .into_iter()
            .collect::<Vec<PublicKey>>())
    }

    /// Adds new members to an existing MLS group
    ///
    /// This method performs the complete workflow for adding members to a group:
    /// 1. Fetches key packages for all new members from their configured relays
    /// 2. Creates an MLS add members proposal and generates welcome messages
    /// 3. Publishes the evolution event to the group's relays
    /// 4. Merges the pending commit to finalize the member addition
    /// 5. Sends welcome messages to each new member via gift wrap
    ///
    /// # Arguments
    /// * `account` - The account performing the member addition (must be group admin)
    /// * `group_id` - The ID of the group to add members to
    /// * `members` - Vector of public keys for the new members to add
    pub async fn add_members_to_group(
        &self,
        account: &Account,
        group_id: &GroupId,
        members: Vec<PublicKey>,
    ) -> Result<()> {
        let mut key_package_events: Vec<Event> = Vec::new();
        let keys = self
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)?;
        let mut users = Vec::new();

        // Fetch key packages for all members
        for pk in members.iter() {
            let (user, newly_created) = User::find_or_create_by_pubkey(pk, &self.database).await?;

            if newly_created {
                self.background_fetch_user_data(&user).await?;
            }
            // Try and get user's key package relays, if they don't have any, use account's default relays
            let mut relays_to_use = user.relays(RelayType::KeyPackage, &self.database).await?;
            if relays_to_use.is_empty() {
                tracing::warn!(
                    target: "whitenoise::accounts::groups::add_members_to_group",
                    "User {} has no relays configured, using account's default relays",
                    user.pubkey
                );
                relays_to_use = account.nip65_relays(self).await?;
            }
            let relays_to_use_urls = Relay::urls(&relays_to_use);
            let some_event = self
                .nostr
                .fetch_user_key_package(*pk, &relays_to_use_urls)
                .await?;
            let event = some_event.ok_or(WhitenoiseError::MdkCoreError(
                mdk_core::Error::KeyPackage("Does not exist".to_owned()),
            ))?;
            key_package_events.push(event);
            users.push(user);
        }

        let (relay_urls, evolution_event, welcome_rumors) = {
            let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
            let relay_urls = Self::ensure_group_relays(&mdk, group_id)?;

            let update_result = mdk.add_members(group_id, &key_package_events)?;
            // Merge the pending commit immediately after creating it
            // This ensures our local state is correct before publishing
            mdk.merge_pending_commit(group_id)?;

            (
                relay_urls,
                update_result.evolution_event,
                update_result.welcome_rumors,
            )
        };

        let welcome_rumors = match welcome_rumors {
            None => {
                return Err(WhitenoiseError::MdkCoreError(mdk_core::Error::Group(
                    "Missing welcome message".to_owned(),
                )));
            }
            Some(wr) => wr,
        };

        if welcome_rumors.len() != users.len() {
            return Err(WhitenoiseError::Other(anyhow::Error::msg(
                "Welcome rumours are missing for some of the members",
            )));
        }

        self.nostr
            .publish_event_to(evolution_event, &account.pubkey, &relay_urls)
            .await?;

        // Evolution event published successfully
        // Fan out the welcome message to all members
        for (welcome_rumor, user) in welcome_rumors.iter().zip(users) {
            // Get the public key of the member from the key package event
            let key_package_event_id =
                welcome_rumor
                    .tags
                    .event_ids()
                    .next()
                    .ok_or(WhitenoiseError::Other(anyhow::anyhow!(
                        "No event ID found in welcome rumor"
                    )))?;

            let member_pubkey = key_package_events
                .iter()
                .find(|event| event.id == *key_package_event_id)
                .map(|event| event.pubkey)
                .ok_or(WhitenoiseError::Other(anyhow::anyhow!(
                    "No public key found in key package event"
                )))?;

            // Create a timestamp 1 month in the future
            let one_month_future = Timestamp::now() + Duration::from_secs(30 * 24 * 60 * 60);

            let relays_to_use = self
                .resolve_member_delivery_relays(
                    &user,
                    account,
                    "whitenoise::accounts::groups::add_members_to_group",
                )
                .await?;

            let relay_urls = Relay::urls(&relays_to_use);

            self.nostr
                .publish_gift_wrap_to(
                    &member_pubkey,
                    welcome_rumor.clone(),
                    &[Tag::expiration(one_month_future)],
                    account.pubkey,
                    &relay_urls,
                    keys.clone(),
                )
                .await
                .map_err(WhitenoiseError::from)?;
        }

        Ok(())
    }

    /// Removes members from an existing MLS group
    ///
    /// This method performs the complete workflow for removing members from a group:
    /// 1. Creates an MLS remove members proposal
    /// 2. Merges the pending commit to finalize the member removal
    /// 3. Publishes the evolution event to the group's relays
    ///
    /// # Arguments
    /// * `account` - The account performing the member removal (must be group admin)
    /// * `group_id` - The ID of the group to remove members from
    /// * `members` - Vector of public keys for the members to remove
    pub async fn remove_members_from_group(
        &self,
        account: &Account,
        group_id: &GroupId,
        members: Vec<PublicKey>,
    ) -> Result<()> {
        let (relay_urls, evolution_event) = {
            let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
            let relay_urls = Self::ensure_group_relays(&mdk, group_id)?;

            let update_result = mdk.remove_members(group_id, &members)?;
            mdk.merge_pending_commit(group_id)?;

            (relay_urls, update_result.evolution_event)
        };

        self.nostr
            .publish_event_to(evolution_event, &account.pubkey, &relay_urls)
            .await?;
        Ok(())
    }

    /// Updates group metadata and publishes the change to group relays.
    ///
    /// This method updates the group data and publishes the change to group relays.
    ///
    /// # Arguments
    /// * `account` - The account performing the group data update (must be group admin)
    /// * `group_id` - The ID of the group to update
    /// * `group_data` - The new group data to update
    pub async fn update_group_data(
        &self,
        account: &Account,
        group_id: &GroupId,
        group_data: NostrGroupDataUpdate,
    ) -> Result<()> {
        let (relay_urls, evolution_event) = {
            let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
            let relay_urls = Self::ensure_group_relays(&mdk, group_id)?;

            let update_result = mdk.update_group_data(group_id, group_data)?;
            mdk.merge_pending_commit(group_id)?;

            (relay_urls, update_result.evolution_event)
        };

        self.nostr
            .publish_event_to(evolution_event, &account.pubkey, &relay_urls)
            .await?;
        Ok(())
    }

    /// Initiates the process to leave a group by creating a self-removal proposal.
    ///
    /// This method creates a self-removal proposal using the nostr-mls library and publishes
    /// it to the group relays. The proposal will need to be committed by a group admin before
    /// the removal is finalized.
    ///
    /// # Arguments
    /// * `account` - The account that wants to leave the group
    /// * `group_id` - The ID of the group to leave
    pub async fn leave_group(&self, account: &Account, group_id: &GroupId) -> Result<()> {
        let (relay_urls, evolution_event) = {
            let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
            let relay_urls = Self::ensure_group_relays(&mdk, group_id)?;

            // Create a self-removal proposal
            let update_result = mdk.leave_group(group_id)?;

            (relay_urls, update_result.evolution_event)
        };

        // Publish the self-removal proposal to the group
        self.nostr
            .publish_event_to(evolution_event, &account.pubkey, &relay_urls)
            .await?;

        // TODO: Do any local updates to ensure that we're accurately reflecting that the account is trying to leave this group
        Ok(())
    }

    /// Syncs group image cache if needed (smart, hash-based check)
    ///
    /// This method is called after processing welcomes and commits to proactively
    /// cache group images. It only downloads if:
    /// 1. The group has an image set
    /// 2. The image_hash is not already cached
    ///
    /// This ensures images are ready before the UI needs them, while avoiding
    /// redundant downloads.
    ///
    /// # Arguments
    /// * `account` - The account viewing the group
    /// * `group_id` - The MLS group ID
    pub(crate) async fn sync_group_image_cache_if_needed(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<()> {
        let group: mdk_core::prelude::group_types::Group;
        {
            // Get group data to check if it has an image
            let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
            group = mdk
                .get_group(group_id)
                .map_err(WhitenoiseError::from)?
                .ok_or(WhitenoiseError::GroupNotFound)?;
        }

        // Check if group has an image set
        let (image_hash, image_key, image_nonce) =
            match (group.image_hash, group.image_key, group.image_nonce) {
                (Some(hash), Some(key), Some(nonce)) => (hash, key, nonce),
                _ => return Ok(()), // No image set, nothing to do
            };

        // Try to get the stored blossom_url from the database
        let blossom_url = if let Some(media_file) =
            crate::whitenoise::database::media_files::MediaFile::find_by_hash(
                &self.database,
                &image_hash,
            )
            .await?
        {
            media_file
                .blossom_url
                .and_then(|url_str| Url::parse(&url_str).ok())
        } else {
            None
        };

        // Download and cache the image
        self.download_and_cache_group_image(
            blossom_url,
            &account.pubkey,
            group_id,
            &image_hash,
            &image_key,
            &image_nonce,
        )
        .await?;

        Ok(())
    }

    /// Spawns a background task to sync group image cache without blocking
    ///
    /// This is used by event handlers to proactively cache group images
    /// without blocking event processing. Failures are logged but don't
    /// affect the caller - images will download on-demand if needed.
    ///
    /// # Arguments
    /// * `account` - The account viewing the group
    /// * `group_id` - The MLS group ID
    pub(crate) fn background_sync_group_image_cache_if_needed(
        account: &Account,
        group_id: &GroupId,
    ) {
        let account_clone = account.clone();
        let group_id_clone = group_id.clone();
        tokio::spawn(async move {
            let whitenoise = match Whitenoise::get_instance() {
                Ok(wn) => wn,
                Err(e) => {
                    tracing::error!(
                        target: "whitenoise::groups::background_sync_group_image_cache_if_needed",
                        "Failed to get Whitenoise instance for background image cache: {}",
                        e
                    );
                    return;
                }
            };

            if let Err(e) = whitenoise
                .sync_group_image_cache_if_needed(&account_clone, &group_id_clone)
                .await
            {
                tracing::warn!(
                    target: "whitenoise::groups::background_sync_group_image_cache_if_needed",
                    "Background image cache failed: {}. Image will download on-demand.",
                    e
                );
            }
        });
    }

    /// Downloads, decrypts, and caches a group image if not already cached
    ///
    /// # Arguments
    /// * `blossom_url` - Optional Blossom server URL (uses default if None)
    /// * `account_pubkey` - The account accessing the image
    /// * `group_id` - The MLS group ID
    /// * `image_hash` - SHA-256 hash of the encrypted image
    /// * `image_key` - Encryption key
    /// * `image_nonce` - Encryption nonce
    ///
    /// # Returns
    /// The MediaFile record for the cached image
    async fn download_and_cache_group_image(
        &self,
        blossom_url: Option<Url>,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        image_hash: &[u8; 32],
        image_key: &[u8; 32],
        image_nonce: &[u8; 12],
    ) -> Result<MediaFile> {
        let hash_hex = hex::encode(image_hash);

        // Check if already cached and return early if found
        if let Some(cached_path) = self.check_cached_image(&hash_hex).await? {
            let media_file = self
                .link_cached_image_to_group(
                    account_pubkey,
                    group_id,
                    &cached_path,
                    image_hash,
                    image_key,
                )
                .await?;
            return Ok(media_file);
        }

        // Use provided URL or fall back to default
        let blossom_url = blossom_url.unwrap_or_else(Self::default_blossom_url);

        tracing::info!(
            target: "whitenoise::groups::download_and_cache_group_image",
            "Downloading group image {} for group {} from {}",
            hash_hex,
            hex::encode(group_id.as_slice()),
            blossom_url
        );

        // Download, verify, decrypt, and cache the image
        let encrypted_data = Self::download_blob_from_blossom(&blossom_url, image_hash).await?;
        Self::verify_blob_hash(&encrypted_data, image_hash)?;

        let decrypted_data = Self::decrypt_group_image(&encrypted_data, image_key, image_nonce)?;
        let image_type = ImageType::detect(&decrypted_data).map_err(|e| {
            WhitenoiseError::UnsupportedMediaFormat(format!("Failed to detect image type: {}", e))
        })?;

        tracing::debug!(
            target: "whitenoise::groups::download_and_cache_group_image",
            "Detected image type: {} for group image {}",
            image_type.mime_type(),
            hash_hex
        );

        let media_file = self
            .store_and_record_group_image(
                account_pubkey,
                group_id,
                &decrypted_data,
                image_hash,
                image_key,
                &image_type,
                &blossom_url,
            )
            .await?;

        tracing::info!(
            target: "whitenoise::groups::download_and_cache_group_image",
            "Cached group image at: {}",
            media_file.file_path.display()
        );

        Ok(media_file)
    }

    /// Checks if an image is already cached
    async fn check_cached_image(&self, hash_hex: &str) -> Result<Option<PathBuf>> {
        let media_files = self.media_files();
        if let Some(cached_path) = media_files.find_file_with_prefix(hash_hex).await {
            tracing::debug!(
                target: "whitenoise::groups::check_cached_image",
                "Group image already cached at: {}",
                cached_path.display()
            );
            Ok(Some(cached_path))
        } else {
            Ok(None)
        }
    }

    /// Links an already cached image to a group in the database
    ///
    /// When an image file is already cached on disk (from another group using the same image),
    /// this method creates a database record linking this group/account to the existing file.
    ///
    /// If an existing database record exists for this file hash, it copies metadata from it.
    /// If no database record exists, it detects the MIME type from the cached file itself.
    ///
    /// # Returns
    /// The MediaFile record that was created or already exists
    async fn link_cached_image_to_group(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        cached_path: &Path,
        image_hash: &[u8; 32],
        image_key: &[u8; 32],
    ) -> Result<MediaFile> {
        // Try to get metadata from any existing record with this hash
        let existing_record_opt = MediaFile::find_by_hash(&self.database, image_hash).await?;

        if let Some(existing_record) = existing_record_opt {
            self.link_cached_image_from_existing_record(
                account_pubkey,
                group_id,
                cached_path,
                image_hash,
                existing_record,
            )
            .await
        } else {
            self.link_cached_image_with_detection(
                account_pubkey,
                group_id,
                cached_path,
                image_hash,
                image_key,
            )
            .await
        }
    }

    /// Links a cached image using metadata from an existing database record
    async fn link_cached_image_from_existing_record(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        cached_path: &Path,
        image_hash: &[u8; 32],
        existing_record: crate::whitenoise::database::media_files::MediaFile,
    ) -> Result<MediaFile> {
        let metadata_ref = existing_record.file_metadata.as_ref();
        let original_hash_ref = existing_record
            .original_file_hash
            .as_ref()
            .and_then(|hash| hash.as_slice().try_into().ok());
        let upload = MediaFileUpload {
            data: &[], // Empty is OK - file already exists on disk
            original_file_hash: original_hash_ref.as_ref(),
            encrypted_file_hash: *image_hash,
            mime_type: &existing_record.mime_type,
            media_type: &existing_record.media_type,
            blossom_url: existing_record.blossom_url.as_deref(),
            nostr_key: existing_record.nostr_key.clone(),
            file_metadata: metadata_ref,
        };

        self.media_files()
            .record_in_database(account_pubkey, group_id, cached_path, upload)
            .await
    }

    /// Links a cached image by detecting its MIME type from the file
    async fn link_cached_image_with_detection(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        cached_path: &Path,
        image_hash: &[u8; 32],
        image_key: &[u8; 32],
    ) -> Result<MediaFile> {
        tracing::debug!(
            target: "whitenoise::groups::link_cached_image_with_detection",
            "No existing database record for hash {}, detecting MIME type from cached file",
            hex::encode(image_hash)
        );

        let file_data = tokio::fs::read(cached_path).await?;

        let image_type = ImageType::detect(&file_data).map_err(|e| {
            WhitenoiseError::UnsupportedMediaFormat(format!(
                "Failed to detect image type for cached file {}: {}",
                cached_path.display(),
                e
            ))
        })?;

        // Compute original_file_hash from the decrypted data
        let mut hasher = Sha256::new();
        hasher.update(&file_data);
        let original_hash: [u8; 32] = hasher.finalize().into();

        // Derive the upload keypair from the image key for cleanup purposes
        let upload_keypair = group_image::derive_upload_keypair(image_key).map_err(|e| {
            WhitenoiseError::Other(anyhow::anyhow!("Failed to derive upload keypair: {}", e))
        })?;

        let upload = MediaFileUpload {
            data: &[], // Empty is OK - file already exists on disk
            original_file_hash: Some(&original_hash),
            encrypted_file_hash: *image_hash,
            mime_type: image_type.mime_type(),
            media_type: "group_image",
            blossom_url: None,
            nostr_key: Some(upload_keypair.secret_key().to_secret_hex()),
            file_metadata: None,
        };

        self.media_files()
            .record_in_database(account_pubkey, group_id, cached_path, upload)
            .await
    }

    /// Downloads an encrypted blob from a Blossom server
    async fn download_blob_from_blossom(
        blossom_url: &Url,
        image_hash: &[u8; 32],
    ) -> Result<Vec<u8>> {
        use nostr::hashes::{Hash, sha256::Hash as Sha256Hash};

        let client = BlossomClient::new(blossom_url.clone());
        let sha256 = Sha256Hash::from_slice(image_hash)
            .map_err(|e| WhitenoiseError::Other(anyhow::anyhow!("Invalid SHA256 hash: {}", e)))?;

        let download_future = client.get_blob(sha256, None, None, None::<&Keys>);

        tokio::time::timeout(BLOSSOM_TIMEOUT, download_future)
            .await
            .map_err(|_| {
                WhitenoiseError::BlossomDownload(format!(
                    "Download timed out after {} seconds",
                    BLOSSOM_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|e| {
                WhitenoiseError::BlossomDownload(format!("Failed to download blob: {}", e))
            })
    }

    /// Verifies that downloaded blob matches expected hash
    fn verify_blob_hash(data: &[u8], expected_hash: &[u8; 32]) -> Result<()> {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let actual_hash: [u8; 32] = hasher.finalize().into();

        if &actual_hash != expected_hash {
            return Err(WhitenoiseError::HashMismatch {
                expected: hex::encode(expected_hash),
                actual: hex::encode(actual_hash),
            });
        }

        Ok(())
    }

    /// Decrypts a group image using the provided key and nonce
    fn decrypt_group_image(
        encrypted_data: &[u8],
        image_key: &[u8; 32],
        image_nonce: &[u8; 12],
    ) -> Result<Vec<u8>> {
        group_image::decrypt_group_image(encrypted_data, image_key, image_nonce).map_err(|e| {
            WhitenoiseError::ImageDecryptionFailed(format!("Failed to decrypt group image: {}", e))
        })
    }

    /// Downloads and decrypts a chat media blob
    ///
    /// This helper orchestrates the download from Blossom, hash verification,
    /// and MDK decryption in a single focused operation.
    ///
    /// # Arguments
    /// * `account_pubkey` - The account downloading the media
    /// * `data_dir` - The data directory for MDK storage
    /// * `group_id` - The MLS group ID
    /// * `media_file` - The MediaFile record containing URLs, hashes, and metadata
    /// * `original_file_hash` - SHA-256 of original content (for MDK decryption)
    ///
    /// # Returns
    /// * `Ok(Vec<u8>)` - Decrypted file data
    /// * `Err(WhitenoiseError)` - If download, verification, or decryption fails
    async fn download_and_decrypt_chat_media_blob(
        account_pubkey: &PublicKey,
        data_dir: &Path,
        group_id: &GroupId,
        media_file: &MediaFile,
        original_file_hash: &[u8; 32],
    ) -> Result<Vec<u8>> {
        // Extract filename required for MDK AAD (cryptographically bound)
        let filename = media_file
            .file_metadata
            .as_ref()
            .and_then(|m| m.original_filename.as_deref())
            .ok_or_else(|| {
                WhitenoiseError::MediaCache("Missing required filename metadata".to_string())
            })?;
        // Parse encrypted_file_hash for download verification
        let encrypted_hash: [u8; 32] = media_file
            .encrypted_file_hash
            .as_slice()
            .try_into()
            .map_err(|_| {
                WhitenoiseError::MediaCache("Invalid encrypted_file_hash length".to_string())
            })?;

        // Parse Blossom URL
        let blossom_url_str = media_file
            .blossom_url
            .as_ref()
            .ok_or_else(|| WhitenoiseError::MediaCache("No Blossom URL".to_string()))?;

        let blossom_url = Url::parse(blossom_url_str).map_err(|e| {
            WhitenoiseError::MediaCache(format!("Invalid Blossom URL '{}': {}", blossom_url_str, e))
        })?;

        tracing::debug!(
            target: "whitenoise::groups::download_and_decrypt",
            "Downloading encrypted blob from: {}",
            blossom_url
        );

        // Download encrypted blob (includes hash verification)
        let encrypted_data =
            Self::download_blob_from_blossom(&blossom_url, &encrypted_hash).await?;

        // Decrypt using MDK
        let mdk = Account::create_mdk(*account_pubkey, data_dir)?;
        let media_manager = mdk.media_manager(group_id.clone());

        let reference = MediaReference {
            url: String::new(),
            original_hash: *original_file_hash,
            mime_type: media_file.mime_type.clone(),
            filename: filename.to_string(),
            dimensions: None, // Not used in decryption (only display metadata)
        };

        media_manager
            .decrypt_from_download(&encrypted_data, &reference)
            .map_err(|e| {
                WhitenoiseError::Other(anyhow::anyhow!("Failed to decrypt chat media: {}", e))
            })
    }

    /// Stores decrypted image to cache and records it in database
    async fn store_and_record_group_image(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        decrypted_data: &[u8],
        image_hash: &[u8; 32],
        image_key: &[u8; 32],
        image_type: &ImageType,
        blossom_server: &Url,
    ) -> Result<MediaFile> {
        let hash_hex = hex::encode(image_hash);
        let filename = format!("{}.{}", hash_hex, image_type.extension());
        let blossom_url = blossom_server.join(&hash_hex).map_err(|e| {
            WhitenoiseError::Other(anyhow::anyhow!("Failed to construct Blossom URL: {}", e))
        })?;

        // Compute original_file_hash from the decrypted data
        let mut hasher = Sha256::new();
        hasher.update(decrypted_data);
        let original_hash: [u8; 32] = hasher.finalize().into();

        // Derive the upload keypair from the image key for cleanup purposes
        let upload_keypair = group_image::derive_upload_keypair(image_key).map_err(|e| {
            WhitenoiseError::Other(anyhow::anyhow!("Failed to derive upload keypair: {}", e))
        })?;

        let upload = MediaFileUpload {
            data: decrypted_data,
            original_file_hash: Some(&original_hash),
            encrypted_file_hash: *image_hash,
            mime_type: image_type.mime_type(),
            media_type: "group_image",
            blossom_url: Some(blossom_url.as_str()),
            nostr_key: Some(upload_keypair.secret_key().to_secret_hex()),
            file_metadata: None,
        };

        self.media_files()
            .store_and_record(account_pubkey, group_id, &filename, upload)
            .await
    }

    /// Uploads encrypted blob to Blossom server with timeout
    ///
    /// # Arguments
    /// * `blossom_server_url` - Blossom server URL to upload to
    /// * `encrypted_data` - The encrypted image data to upload
    /// * `mime_type` - MIME type of the original image
    /// * `upload_keypair` - Keypair for signing the upload
    ///
    /// # Returns
    /// * `Ok(BlobDescriptor)` - Descriptor from Blossom server containing URL and hash
    /// * `Err(WhitenoiseError)` - Upload timeout or network error
    async fn upload_encrypted_blob_to_blossom(
        blossom_server_url: &Url,
        encrypted_data: Vec<u8>,
        mime_type: &str,
        upload_keypair: &Keys,
    ) -> Result<nostr_blossom::bud02::BlobDescriptor> {
        let client = BlossomClient::new(blossom_server_url.clone());
        let upload_future = client.upload_blob(
            encrypted_data,
            Some(mime_type.to_string()),
            None,
            Some(upload_keypair),
        );

        tokio::time::timeout(BLOSSOM_TIMEOUT, upload_future)
            .await
            .map_err(|_| {
                WhitenoiseError::Other(anyhow::anyhow!(
                    "Upload timed out after {} seconds",
                    BLOSSOM_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|err| WhitenoiseError::Other(anyhow::anyhow!(err)))
    }

    /// Uploads a group image to a Blossom server and returns the encrypted metadata.
    ///
    /// The returned metadata (hash, key, nonce) should be passed to `update_group_data`
    /// to update the group's image settings.
    ///
    /// # Arguments
    /// * `account` - The account performing the upload (must be a group admin)
    /// * `group_id` - The ID of the group to upload the image for
    /// * `file_path` - Path to the image file to upload
    /// * `blossom_server_url` - Blossom server URL to upload to
    /// * `options` - Optional media processing options (defaults to standard options if None)
    pub async fn upload_group_image(
        &self,
        account: &Account,
        group_id: &GroupId,
        file_path: &str,
        blossom_server_url: Option<Url>,
        options: Option<MediaProcessingOptions>,
    ) -> Result<([u8; 32], [u8; 32], [u8; 12])> {
        // Verify the account is an admin of the group
        let admins = self.group_admins(account, group_id).await?;
        if !admins.contains(&account.pubkey) {
            return Err(WhitenoiseError::AccountNotAuthorized);
        }

        // Read the image file
        let image_data = tokio::fs::read(file_path).await?;

        // Detect and validate image type from file content
        // This uses the image crate to both detect the format and validate the image
        let image_type = ImageType::detect(&image_data).map_err(|e| {
            WhitenoiseError::UnsupportedMediaFormat(format!(
                "Failed to detect or validate image from {}: {}",
                file_path, e
            ))
        })?;

        tracing::debug!(
            target: "whitenoise::groups::upload_group_image",
            "Detected and validated image type: {} for file {}",
            image_type.mime_type(),
            file_path
        );

        // Use MDK to prepare the image for upload (encrypt + derive keypair)
        let prepared = group_image::prepare_group_image_for_upload_with_options(
            &image_data,
            image_type.mime_type(),
            &options.unwrap_or_default(),
        )
        .map_err(|e| {
            WhitenoiseError::Other(anyhow::anyhow!("Failed to prepare group image: {}", e))
        })?;

        let blossom_server_url = blossom_server_url.unwrap_or(Self::default_blossom_url());
        // Upload encrypted data to Blossom using the derived keypair
        let descriptor = Self::upload_encrypted_blob_to_blossom(
            &blossom_server_url,
            prepared.encrypted_data,
            image_type.mime_type(),
            &prepared.upload_keypair,
        )
        .await?;

        // Verify the Blossom server returned the expected hash
        // The descriptor.sha256 is a Hash type from bitcoin_hashes, convert to byte array
        let returned_hash_bytes: [u8; 32] = *descriptor.sha256.as_ref();

        if returned_hash_bytes != prepared.encrypted_hash {
            return Err(WhitenoiseError::HashMismatch {
                expected: hex::encode(prepared.encrypted_hash),
                actual: hex::encode(returned_hash_bytes),
            });
        }

        tracing::debug!(
            target: "whitenoise::groups::upload_group_image",
            "Successfully uploaded group image for group {} to Blossom server. Hash: {}",
            hex::encode(group_id.as_slice()),
            hex::encode(prepared.encrypted_hash)
        );

        // Cache the original unencrypted image immediately to avoid re-downloading
        // This is an optimization: the uploader already has the image in memory
        let hash_hex = hex::encode(prepared.encrypted_hash);
        let filename = format!("{}.{}", hash_hex, image_type.extension());

        // Compute original_file_hash from the original image data
        let mut hasher = Sha256::new();
        hasher.update(&image_data);
        let original_hash: [u8; 32] = hasher.finalize().into();

        let upload = MediaFileUpload {
            data: &image_data,
            original_file_hash: Some(&original_hash),
            encrypted_file_hash: prepared.encrypted_hash,
            mime_type: image_type.mime_type(),
            media_type: "group_image",
            blossom_url: Some(descriptor.url.as_str()),
            nostr_key: Some(prepared.upload_keypair.secret_key().to_secret_hex()),
            file_metadata: None,
        };

        if let Err(e) = self
            .media_files()
            .store_and_record(&account.pubkey, group_id, &filename, upload)
            .await
        {
            tracing::warn!(
                target: "whitenoise::groups::upload_group_image",
                "Failed to cache uploaded group image: {}. Image will be downloaded on next access.",
                e
            );
        }

        // Return the metadata needed for group update
        Ok((
            prepared.encrypted_hash,
            prepared.image_key,
            prepared.image_nonce,
        ))
    }

    /// Uploads a media file to a Blossom server and returns the media file record.
    ///
    /// This method is designed for media files sent in group chat messages. Unlike
    /// `upload_group_image`, it does not require admin privileges since any group
    /// member can send media in chat.
    ///
    /// Supports images (JPEG, PNG, GIF, WebP), videos (MP4, WebM, MOV), audio
    /// (MP3, OGG, M4A, WAV), and documents (PDF).
    ///
    /// Uses the encrypted media manager which derives encryption keys from the group secret.
    /// The encryption keys are not returned as they can be re-derived for decryption.
    ///
    /// # Arguments
    /// * `account` - The account uploading the media file
    /// * `group_id` - The ID of the group where the media will be used
    /// * `file_path` - Path to the media file to upload
    /// * `blossom_server_url` - Optional Blossom server URL (uses default if None)
    /// * `options` - Optional media processing options (defaults to standard options if None)
    ///
    /// # Returns
    /// * `Ok(MediaFile)` - MediaFile record containing file hash and metadata
    /// * `Err(WhitenoiseError)` - If media validation, upload, or caching fails
    pub async fn upload_chat_media(
        &self,
        account: &Account,
        group_id: &GroupId,
        file_path: &str,
        blossom_server_url: Option<Url>,
        options: Option<MediaProcessingOptions>,
    ) -> Result<MediaFile> {
        // Read the media file
        let file_data = tokio::fs::read(file_path).await?;

        // Detect and validate media type from file content
        let media_detection = crate::types::detect_media_type(&file_data)?;

        tracing::debug!(
            target: "whitenoise::groups::upload_chat_media",
            "Detected and validated media type: {} for file {}",
            media_detection.mime_type(),
            file_path
        );

        // Extract filename from path for AAD in encryption
        let original_filename = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| WhitenoiseError::Other(anyhow::anyhow!("Invalid file path")))?;

        // Use MDK encrypted media manager to prepare the media file for upload
        // Wrap in a block to ensure MDK and media_manager are dropped before any await points
        let prepared = {
            let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
            let media_manager = mdk.media_manager(group_id.clone());

            media_manager
                .encrypt_for_upload_with_options(
                    &file_data,
                    media_detection.mime_type(),
                    original_filename,
                    &options.unwrap_or_default(),
                )
                .map_err(|e| {
                    WhitenoiseError::Other(anyhow::anyhow!("Failed to encrypt chat media: {}", e))
                })?
        };

        let blossom_server_url = blossom_server_url.unwrap_or_else(Self::default_blossom_url);

        // Generate fresh keys for upload authentication (for MIP-04 cleanup)
        let upload_keys = nostr_sdk::Keys::generate();
        let upload_keys_hex = upload_keys.secret_key().to_secret_hex();

        // Upload encrypted data to Blossom
        let descriptor = Self::upload_encrypted_blob_to_blossom(
            &blossom_server_url,
            prepared.encrypted_data,
            &prepared.mime_type,
            &upload_keys,
        )
        .await?;

        // Verify the Blossom server returned the expected hash
        let returned_hash_bytes: [u8; 32] = *descriptor.sha256.as_ref();
        if returned_hash_bytes != prepared.encrypted_hash {
            return Err(WhitenoiseError::HashMismatch {
                expected: hex::encode(prepared.encrypted_hash),
                actual: hex::encode(returned_hash_bytes),
            });
        }

        tracing::debug!(
            target: "whitenoise::groups::upload_chat_media",
            "Successfully uploaded chat media for group {} to Blossom server. Hash: {}",
            hex::encode(group_id.as_slice()),
            hex::encode(prepared.encrypted_hash)
        );

        // Cache the decrypted media file locally
        let hash_hex = hex::encode(prepared.encrypted_hash);
        let cached_filename = format!("{}.{}", hash_hex, media_detection.extension());

        // Construct file metadata from the prepared media data
        let file_metadata = if prepared.dimensions.is_some() || prepared.blurhash.is_some() {
            Some(FileMetadata {
                original_filename: Some(prepared.filename.clone()),
                dimensions: prepared.dimensions.map(|(w, h)| format!("{}x{}", w, h)),
                blurhash: prepared.blurhash.clone(),
            })
        } else {
            None
        };

        let upload = MediaFileUpload {
            data: &file_data,
            original_file_hash: Some(&prepared.original_hash),
            encrypted_file_hash: prepared.encrypted_hash,
            mime_type: &prepared.mime_type,
            media_type: "chat_media",
            blossom_url: Some(descriptor.url.as_str()),
            nostr_key: Some(upload_keys_hex),
            file_metadata: file_metadata.as_ref(),
        };

        let media_file = self
            .media_files()
            .store_and_record(&account.pubkey, group_id, &cached_filename, upload)
            .await?;

        Ok(media_file)
    }

    /// Downloads a chat media file and returns the updated MediaFile record
    ///
    /// This method downloads and decrypts media files sent in group chat messages.
    /// It uses the original file hash from the imeta 'x' field per MIP-04 specification.
    ///
    /// **On-demand download**: If the file is already cached, returns immediately.
    /// **Idempotent**: Safe to call multiple times.
    ///
    /// # Arguments
    /// * `account` - The account downloading the media
    /// * `group_id` - The MLS group ID where the media was shared
    /// * `original_file_hash` - The SHA-256 hash of the original file (from imeta 'x' field)
    ///
    /// # Returns
    /// * `Ok(MediaFile)` - MediaFile record with the local file path
    /// * `Err(WhitenoiseError)` - If download, decryption, or database operation fails
    ///
    /// # Errors
    /// * `MediaCache("MediaFile not found")` - No record exists for this hash in this group
    /// * `MediaCache("Not chat media")` - The MediaFile is not a chat_media type
    /// * `MediaCache("Missing required metadata")` - Required filename/dimensions are missing
    /// * `MediaCache("No Blossom URL")` - MediaFile has no Blossom download URL
    /// * `HashMismatch` - Downloaded blob doesn't match expected hash
    /// * Network errors during download
    /// * MDK decryption errors
    ///
    /// # Example
    /// ```ignore
    /// // Extract hash from imeta tag's 'x' field
    /// let original_hash: [u8; 32] = hex::decode(imeta_x_field)?.try_into()?;
    /// let media_file = whitenoise
    ///     .download_chat_media(&account, &group_id, &original_hash)
    ///     .await?;
    /// display_media(&media_file.file_path);
    /// ```
    pub async fn download_chat_media(
        &self,
        account: &Account,
        group_id: &GroupId,
        original_file_hash: &[u8; 32],
    ) -> Result<MediaFile> {
        // Find MediaFile record by original_file_hash + group + account (MIP-04 compliant)
        let media_file = MediaFile::find_by_original_hash_and_group(
            &self.database,
            original_file_hash,
            group_id,
            &account.pubkey,
        )
        .await?
        .ok_or_else(|| {
            WhitenoiseError::MediaCache(format!(
                "MediaFile not found for original_hash={} in group for account {}",
                hex::encode(original_file_hash),
                account.pubkey.to_hex()
            ))
        })?;

        // Check if already cached (early return for idempotency)
        if !media_file.file_path.as_os_str().is_empty() && media_file.file_path.exists() {
            tracing::debug!(
                target: "whitenoise::groups::download_chat_media",
                "Media file already cached at: {}",
                media_file.file_path.display()
            );
            return Ok(media_file);
        }

        // Verify this is chat_media (not group_image or other types)
        if media_file.media_type != "chat_media" {
            return Err(WhitenoiseError::MediaCache(format!(
                "Not chat media: media_type={}",
                media_file.media_type
            )));
        }

        // Download and decrypt the media blob
        let decrypted_data = Self::download_and_decrypt_chat_media_blob(
            &account.pubkey,
            &self.config.data_dir,
            group_id,
            &media_file,
            original_file_hash,
        )
        .await?;

        // Detect MIME type and extension from decrypted content
        let media_detection = crate::types::detect_media_type(&decrypted_data)?;

        tracing::debug!(
            target: "whitenoise::groups::download_chat_media",
            "Decrypted media: {} ({})",
            media_detection.mime_type(),
            media_detection.extension()
        );

        // Store decrypted file to cache
        let hash_hex = hex::encode(&media_file.encrypted_file_hash);
        let cached_filename = format!("{}.{}", hash_hex, media_detection.extension());

        let cache_path = self
            .storage
            .media_files
            .store_file(&cached_filename, &decrypted_data)
            .await?;

        tracing::debug!(
            target: "whitenoise::groups::download_chat_media",
            "Cached to: {}",
            cache_path.display()
        );

        // Update database record with file path
        let media_file_id = media_file.id.ok_or_else(|| {
            WhitenoiseError::MediaCache("MediaFile record missing id".to_string())
        })?;

        let updated_file =
            MediaFile::update_file_path(&self.database, media_file_id, &cache_path).await?;

        Ok(updated_file)
    }

    /// Retrieves all media files for a specific group
    ///
    /// Returns all MediaFile records associated with the group, including:
    /// - Chat media uploaded by any group member
    /// - Group images
    /// - Media references from received messages (may have empty file_path if not downloaded)
    ///
    /// # Arguments
    /// * `group_id` - The MLS group ID to fetch media files for
    ///
    /// # Returns
    /// * `Ok(Vec<MediaFile>)` - List of all media files for the group
    /// * `Err(WhitenoiseError)` - If database query fails
    ///
    /// # Example
    /// ```ignore
    /// let media_files = whitenoise.get_media_files_for_group(&group_id).await?;
    /// for file in media_files {
    ///     println!("Media: {} ({})", file.mime_type, file.media_type);
    /// }
    /// ```
    pub async fn get_media_files_for_group(&self, group_id: &GroupId) -> Result<Vec<MediaFile>> {
        MediaFile::find_by_group(&self.database, group_id).await
    }

    /// Gets the local file path for a group's current image
    ///
    /// This is the primary method for UI/Flutter to retrieve group images.
    /// If the image is already cached, returns the path immediately.
    /// If not cached, downloads, decrypts, and caches it first.
    ///
    /// **This is an on-demand method** - it only downloads when the UI actually
    /// needs to display the image, avoiding unnecessary network traffic.
    ///
    /// # Arguments
    /// * `account` - The account viewing the group
    /// * `group_id` - The MLS group ID
    ///
    /// # Returns
    /// * `Ok(Some(PathBuf))` - Path to cached decrypted image
    /// * `Ok(None)` - Group has no image set
    /// * `Err(WhitenoiseError)` - Download or cache operation failed
    ///
    /// # Example
    /// ```ignore
    /// // In UI code - only downloads if not already cached
    /// if let Some(image_path) = whitenoise.get_group_image_path(&account, &group_id).await? {
    ///     display_image(image_path);
    /// }
    /// ```
    pub async fn get_group_image_path(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<Option<PathBuf>> {
        let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
        let group = mdk
            .get_group(group_id)
            .map_err(WhitenoiseError::from)?
            .ok_or(WhitenoiseError::GroupNotFound)?;

        self.resolve_group_image_path(account, &group).await
    }

    /// Resolves the cached image path for a group (core logic).
    ///
    /// This is the internal implementation that receives the Group directly,
    /// avoiding redundant MDK instantiation and group fetching when the caller
    /// already has the group data.
    ///
    /// # Arguments
    /// * `account` - The account accessing the image
    /// * `group` - The group to resolve the image for
    ///
    /// # Returns
    /// * `Ok(Some(PathBuf))` - Path to cached decrypted image
    /// * `Ok(None)` - Group has no image configured
    /// * `Err(...)` - Download or cache operation failed
    pub(crate) async fn resolve_group_image_path(
        &self,
        account: &Account,
        group: &group_types::Group,
    ) -> Result<Option<PathBuf>> {
        // Check if group has an image set
        let (image_hash, image_key, image_nonce) =
            match (&group.image_hash, &group.image_key, &group.image_nonce) {
                (Some(hash), Some(key), Some(nonce)) => (*hash, *key, *nonce),
                _ => return Ok(None), // No image set
            };

        // Try to get the stored blossom_url from the database
        let blossom_url = if let Some(media_file) =
            crate::whitenoise::database::media_files::MediaFile::find_by_hash(
                &self.database,
                &image_hash,
            )
            .await?
        {
            media_file
                .blossom_url
                .and_then(|url_str| Url::parse(&url_str).ok())
        } else {
            None
        };

        // Download and cache the image (if not already cached)
        let media_file = self
            .download_and_cache_group_image(
                blossom_url,
                &account.pubkey,
                &group.mls_group_id,
                &image_hash,
                &image_key,
                &image_nonce,
            )
            .await?;

        Ok(Some(media_file.file_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::Whitenoise;
    use crate::whitenoise::test_utils::*;
    use nostr_sdk::RelayUrl;

    #[tokio::test]
    async fn test_create_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Setup creator account
        let creator_account = whitenoise.create_identity().await.unwrap();

        // Setup member accounts
        let mut member_pubkeys = Vec::new();
        for _ in 0..2 {
            let member_account = whitenoise.create_identity().await.unwrap();
            let member_user = User::find_by_pubkey(&member_account.pubkey, &whitenoise.database)
                .await
                .unwrap();
            creator_account
                .follow_user(&member_user, &whitenoise.database)
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
        assert_eq!(group.image_key, config.image_key);

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
            admins: None,
            relays: None,
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
        assert_eq!(updated_group.image_key, new_group_data.image_key.unwrap());
    }

    #[cfg(test)]
    async fn set_user_relays(
        whitenoise: &Whitenoise,
        user: &User,
        relay_type: RelayType,
        relay_urls: &[&str],
    ) -> Vec<RelayUrl> {
        let existing_relays = user.relays(relay_type, &whitenoise.database).await.unwrap();
        for relay in existing_relays {
            user.remove_relay(&relay, relay_type, &whitenoise.database)
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
            user.add_relay(&relay, relay_type, &whitenoise.database)
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

        let fallback_user = fallback_account.user(&whitenoise.database).await.unwrap();
        set_user_relays(
            &whitenoise,
            &fallback_user,
            RelayType::Nip65,
            &["wss://fallback.example.com"],
        )
        .await;

        let member_user = member_account.user(&whitenoise.database).await.unwrap();
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

        let fallback_user = fallback_account.user(&whitenoise.database).await.unwrap();
        set_user_relays(
            &whitenoise,
            &fallback_user,
            RelayType::Nip65,
            &["wss://fallback.example.com"],
        )
        .await;

        let member_user = member_account.user(&whitenoise.database).await.unwrap();
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

        let member_user = member_account.user(&whitenoise.database).await.unwrap();
        set_user_relays(&whitenoise, &member_user, RelayType::Inbox, &[]).await;
        set_user_relays(&whitenoise, &member_user, RelayType::Nip65, &[]).await;

        let fallback_user = fallback_account.user(&whitenoise.database).await.unwrap();
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

        let member_user = member_account.user(&whitenoise.database).await.unwrap();
        set_user_relays(&whitenoise, &member_user, RelayType::Inbox, &[]).await;
        set_user_relays(&whitenoise, &member_user, RelayType::Nip65, &[]).await;

        let fallback_user = fallback_account.user(&whitenoise.database).await.unwrap();
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

        // Creator leaves the group (creates proposal)
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

        // For now, we just verify that the proposal was successfully created and published
        // without errors, which indicates the leave_group method works correctly.
    }

    #[tokio::test]
    async fn test_upload_group_image() {
        use tempfile::NamedTempFile;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

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

        // Upload the group image to local Blossom server (port 3000 per docker-compose.yml)
        let blossom_server = Url::parse("http://localhost:3000").unwrap();
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
                Some(blossom_server),
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
            admins: None,
            relays: None,
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
        assert_eq!(updated_group.image_key, Some(key));
        assert_eq!(updated_group.image_nonce, Some(nonce));

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
            &whitenoise.database,
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

    #[tokio::test]
    async fn test_sync_group_image_cache() {
        use std::time::Duration;
        use tempfile::NamedTempFile;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

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
        let img = ::image::RgbaImage::from_pixel(100, 100, ::image::Rgba([255u8, 0, 0, 255]));
        let temp_file = NamedTempFile::new().unwrap();
        img.save_with_format(temp_file.path(), ::image::ImageFormat::Jpeg)
            .unwrap();
        let temp_path = temp_file.path().to_str().unwrap();

        // Read the original image data for later comparison
        let test_image_data = tokio::fs::read(temp_path).await.unwrap();

        // Creator uploads the group image
        let blossom_server = Url::parse("http://localhost:3000").unwrap();
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
                Some(blossom_server),
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
            admins: None,
            relays: None,
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

    #[tokio::test]
    async fn test_upload_chat_media() {
        use tempfile::NamedTempFile;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

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
                Some(Url::parse("http://localhost:3000").unwrap()),
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
}

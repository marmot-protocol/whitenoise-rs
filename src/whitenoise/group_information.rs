use std::{collections::HashMap, fmt, str::FromStr};

use cgka_traits::types::GroupId as MarmotGroupId;
use chrono::{DateTime, Utc};
use nostr_sdk::PublicKey;
use serde::{Deserialize, Serialize};

use crate::marmot::GroupId;
use crate::marmot::storage::WhitenoiseMarmotStorage;
use crate::perf_instrument;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::database::Database;
use crate::whitenoise::{Whitenoise, WhitenoiseError};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum GroupType {
    #[default]
    Group,
    DirectMessage,
}

impl fmt::Display for GroupType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GroupType::Group => write!(f, "group"),
            GroupType::DirectMessage => write!(f, "direct_message"),
        }
    }
}

impl FromStr for GroupType {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "group" => Ok(GroupType::Group),
            "direct_message" => Ok(GroupType::DirectMessage),
            _ => Err(format!("Invalid group type: {}", s)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupInformation {
    pub id: Option<i64>,
    pub mls_group_id: GroupId,
    pub group_type: GroupType,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl GroupInformation {
    pub(crate) fn infer_group_type_from_group_name(group_name: &str) -> GroupType {
        match group_name {
            "" => GroupType::DirectMessage,
            _ => GroupType::Group,
        }
    }
    /// Creates a new GroupInformation with the specified or inferred group type
    ///
    /// # Arguments
    /// * `mls_group_id` - The MLS group ID
    /// * `group_type` - Optional explicit group type. If None, will be inferred from group name
    /// * `group_name` - The name of the group
    /// * `whitenoise` - Reference to the Whitenoise instance for database operations
    #[perf_instrument("group_info")]
    pub async fn create_for_group(
        whitenoise: &Whitenoise,
        mls_group_id: &GroupId,
        group_type: Option<GroupType>,
        group_name: &str,
    ) -> Result<GroupInformation, WhitenoiseError> {
        let group_type =
            group_type.unwrap_or_else(|| Self::infer_group_type_from_group_name(group_name));
        let (group_info, _was_created) = Self::find_or_create_by_mls_group_id(
            mls_group_id,
            Some(group_type),
            &whitenoise.shared.database,
        )
        .await?;
        Ok(group_info)
    }

    /// Get group information by MLS group ID, repairing it from confirmed Marmot projection if needed.
    #[perf_instrument("group_info")]
    pub async fn get_by_mls_group_id(
        account_pubkey: PublicKey,
        mls_group_id: &GroupId,
        whitenoise: &Whitenoise,
    ) -> Result<GroupInformation, WhitenoiseError> {
        if let Some(group_info) =
            Self::find_or_create_from_marmot_projection(account_pubkey, mls_group_id, whitenoise)
                .await?
        {
            return Ok(group_info);
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    /// Get group information for multiple MLS group IDs
    /// Missing groups will be repaired from confirmed Marmot projections.
    #[perf_instrument("group_info")]
    pub async fn get_by_mls_group_ids(
        account_pubkey: PublicKey,
        mls_group_ids: &[GroupId],
        whitenoise: &Whitenoise,
    ) -> Result<Vec<GroupInformation>, WhitenoiseError> {
        if mls_group_ids.is_empty() {
            return Ok(Vec::new());
        }

        let existing =
            Self::find_by_mls_group_ids(mls_group_ids, &whitenoise.shared.database).await?;

        let mut existing_map: HashMap<GroupId, GroupInformation> = existing
            .into_iter()
            .map(|gi| (gi.mls_group_id.clone(), gi))
            .collect();

        let mut results = Vec::new();
        for mls_group_id in mls_group_ids {
            if let Some(existing_info) = existing_map.remove(mls_group_id) {
                results.push(existing_info);
                continue;
            }

            if let Some(group_info) = Self::find_or_create_from_marmot_projection(
                account_pubkey,
                mls_group_id,
                whitenoise,
            )
            .await?
            {
                results.push(group_info);
                continue;
            }

            return Err(WhitenoiseError::GroupNotFound);
        }

        Ok(results)
    }

    /// Get group information for multiple MLS group IDs using account-scoped
    /// Marmot projection storage.
    #[perf_instrument("group_info")]
    pub(crate) async fn get_by_mls_group_ids_with_marmot_storage(
        mls_group_ids: &[GroupId],
        marmot_storage: &WhitenoiseMarmotStorage,
        database: &Database,
    ) -> Result<Vec<GroupInformation>, WhitenoiseError> {
        let existing = Self::find_by_mls_group_ids(mls_group_ids, database).await?;

        let mut existing_map: HashMap<GroupId, GroupInformation> = existing
            .into_iter()
            .map(|gi| (gi.mls_group_id.clone(), gi))
            .collect();

        let mut results = Vec::new();
        for mls_group_id in mls_group_ids {
            if let Some(existing_info) = existing_map.remove(mls_group_id) {
                results.push(existing_info);
                continue;
            }

            if let Some(group_info) =
                Self::find_or_create_from_marmot_storage(marmot_storage, mls_group_id, database)
                    .await?
            {
                results.push(group_info);
                continue;
            }

            return Err(WhitenoiseError::GroupNotFound);
        }

        Ok(results)
    }

    async fn find_or_create_from_marmot_projection(
        account_pubkey: PublicKey,
        mls_group_id: &GroupId,
        whitenoise: &Whitenoise,
    ) -> Result<Option<GroupInformation>, WhitenoiseError> {
        if let Some(session) = whitenoise.account_manager.get_session(&account_pubkey) {
            return Self::find_or_create_from_marmot_storage(
                &session.marmot_storage,
                mls_group_id,
                &whitenoise.shared.database,
            )
            .await;
        }

        let Some(marmot_storage) = Self::open_existing_marmot_storage(account_pubkey, whitenoise)?
        else {
            return Ok(None);
        };

        Self::find_or_create_from_marmot_storage(
            &marmot_storage,
            mls_group_id,
            &whitenoise.shared.database,
        )
        .await
    }

    fn open_existing_marmot_storage(
        account_pubkey: PublicKey,
        whitenoise: &Whitenoise,
    ) -> Result<Option<WhitenoiseMarmotStorage>, WhitenoiseError> {
        let marmot_storage_path =
            Account::marmot_storage_path(&account_pubkey, &whitenoise.config().data_dir);
        let marmot_db_key_id = Account::marmot_db_key_id(&account_pubkey);

        Ok(WhitenoiseMarmotStorage::open_existing_for_account(
            marmot_storage_path,
            whitenoise.keyring_service_id(),
            &marmot_db_key_id,
        )?)
    }

    async fn find_or_create_from_marmot_storage(
        marmot_storage: &WhitenoiseMarmotStorage,
        mls_group_id: &GroupId,
        database: &Database,
    ) -> Result<Option<GroupInformation>, WhitenoiseError> {
        let marmot_group_id = MarmotGroupId::new(mls_group_id.as_slice().to_vec());
        let Some(projection) = marmot_storage.find_group_projection(&marmot_group_id)? else {
            return Ok(None);
        };

        let group_type = Self::infer_group_type_from_group_name(&projection.name);
        let (group_info, _was_created) =
            Self::find_or_create_by_mls_group_id(mls_group_id, Some(group_type), database).await?;

        Ok(Some(group_info))
    }
}

impl Whitenoise {
    #[perf_instrument("group_info")]
    pub async fn get_group_information_by_mls_group_id(
        &self,
        account_pubkey: PublicKey,
        mls_group_id: &GroupId,
    ) -> Result<GroupInformation, WhitenoiseError> {
        GroupInformation::get_by_mls_group_id(account_pubkey, mls_group_id, self).await
    }

    #[perf_instrument("group_info")]
    pub async fn get_group_information_by_mls_group_ids(
        &self,
        account_pubkey: PublicKey,
        mls_group_ids: &[GroupId],
    ) -> Result<Vec<GroupInformation>, WhitenoiseError> {
        GroupInformation::get_by_mls_group_ids(account_pubkey, mls_group_ids, self).await
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use async_trait::async_trait;
    use cgka_traits::transport::TransportMessage;
    use nostr_sdk::{Keys, RelayUrl};

    use super::*;
    use crate::marmot::GroupConfig;
    use crate::marmot::publish::{MarmotMessagePublisher, MarmotPublishOutcome};
    use crate::whitenoise::accounts::Account;
    use crate::whitenoise::test_utils::{
        assert_obsolete_mls_artifacts_absent, create_mock_whitenoise,
        remove_obsolete_mls_artifacts, setup_unprojected_accepted_group,
        wait_for_key_package_publication,
    };

    #[derive(Clone, Default)]
    struct RecordingMarmotPublisher;

    #[async_trait]
    impl MarmotMessagePublisher for RecordingMarmotPublisher {
        async fn publish(&self, _message: TransportMessage) -> MarmotPublishOutcome {
            MarmotPublishOutcome::Published { accepted_count: 1 }
        }
    }

    #[tokio::test]
    async fn test_create_for_group_with_explicit_type() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[1; 32]);

        let result = GroupInformation::create_for_group(
            &whitenoise,
            &group_id,
            Some(GroupType::DirectMessage),
            "test", // Should be ignored when explicit type provided
        )
        .await;

        assert!(result.is_ok());
        let group_info = result.unwrap();
        assert_eq!(group_info.mls_group_id, group_id);
        assert_eq!(group_info.group_type, GroupType::DirectMessage);
        assert!(group_info.id.is_some());
    }

    #[tokio::test]
    async fn test_create_for_group_with_inferred_type_dm() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[2; 32]);

        let result = GroupInformation::create_for_group(
            &whitenoise,
            &group_id,
            None,
            "", // Should infer DirectMessage
        )
        .await;

        assert!(result.is_ok());
        let group_info = result.unwrap();
        assert_eq!(group_info.mls_group_id, group_id);
        assert_eq!(group_info.group_type, GroupType::DirectMessage);
        assert!(group_info.id.is_some());
    }

    #[tokio::test]
    async fn test_create_for_group_with_inferred_type_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[3; 32]);

        let result = GroupInformation::create_for_group(
            &whitenoise,
            &group_id,
            None,
            "test", // Should infer Group
        )
        .await;

        assert!(result.is_ok());
        let group_info = result.unwrap();
        assert_eq!(group_info.mls_group_id, group_id);
        assert_eq!(group_info.group_type, GroupType::Group);
        assert!(group_info.id.is_some());
    }

    #[tokio::test]
    async fn test_create_for_group_idempotent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[4; 32]);

        // First call - should create
        let result1 = GroupInformation::create_for_group(
            &whitenoise,
            &group_id,
            Some(GroupType::Group),
            "test",
        )
        .await;
        assert!(result1.is_ok());
        let group_info1 = result1.unwrap();

        // Second call - should find existing (not create new)
        let result2 = GroupInformation::create_for_group(
            &whitenoise,
            &group_id,
            Some(GroupType::DirectMessage), // Different type, but should find existing
            "",
        )
        .await;
        assert!(result2.is_ok());
        let group_info2 = result2.unwrap();

        // Should be same record (same ID) and preserve original type
        assert_eq!(group_info1.id, group_info2.id);
        assert_eq!(group_info1.group_type, group_info2.group_type);
        assert_eq!(group_info2.group_type, GroupType::Group); // Original type preserved
    }

    #[tokio::test]
    async fn test_get_by_mls_group_id_creates_with_default() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();

        // Create actual MLS group with default name (non-empty) to infer Group type
        let config =
            crate::whitenoise::test_utils::create_group_config(vec![creator_account.pubkey]);
        let group = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member_account.pubkey], config, None)
            .await
            .unwrap();

        let result = GroupInformation::get_by_mls_group_id(
            creator_account.pubkey,
            &group.mls_group_id,
            &whitenoise,
        )
        .await;

        assert!(result.is_ok());
        let group_info = result.unwrap();
        assert_eq!(group_info.mls_group_id, group.mls_group_id);
        assert_eq!(group_info.group_type, GroupType::Group); // Default type
        assert!(group_info.id.is_some());
    }

    #[tokio::test]
    async fn test_get_by_mls_group_id_finds_existing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();

        // Create actual MLS group with empty name to infer DirectMessage type
        let mut config = crate::whitenoise::test_utils::create_group_config(vec![
            creator_account.pubkey,
            member_account.pubkey,
        ]);
        config.name = "".to_string(); // Empty name for DirectMessage
        let group = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member_account.pubkey], config, None)
            .await
            .unwrap();

        // First create with specific type via create_for_group
        let original = GroupInformation::create_for_group(
            &whitenoise,
            &group.mls_group_id,
            Some(GroupType::DirectMessage),
            "",
        )
        .await
        .unwrap();

        // Get should find the existing one
        let result = GroupInformation::get_by_mls_group_id(
            creator_account.pubkey,
            &group.mls_group_id,
            &whitenoise,
        )
        .await;
        assert!(result.is_ok());
        let found = result.unwrap();

        assert_eq!(original.id, found.id);
        assert_eq!(found.group_type, GroupType::DirectMessage); // Original type preserved
    }

    #[tokio::test]
    async fn test_get_by_mls_group_id_repairs_darkmatter_group_information_without_obsolete_mls_storage()
     {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member]).await;

        let creator_session = whitenoise.require_session(&creator.pubkey).unwrap();
        let config = GroupConfig::new(
            "darkmatter information".to_string(),
            "recreate group information from Darkmatter projection".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("ws://localhost:7777").unwrap()],
            vec![creator.pubkey],
            None,
        );
        let group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member.pubkey],
                config,
                None,
                &RecordingMarmotPublisher,
            )
            .await
            .unwrap();
        let artifacts = remove_obsolete_mls_artifacts(&creator, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        sqlx::query("DELETE FROM group_information WHERE mls_group_id = ?")
            .bind(group.mls_group_id.as_slice())
            .execute(&whitenoise.shared.database.pool)
            .await
            .unwrap();

        let group_info = whitenoise
            .get_group_information_by_mls_group_id(creator.pubkey, &group.mls_group_id)
            .await
            .unwrap();

        assert_eq!(group_info.mls_group_id, group.mls_group_id);
        assert_eq!(group_info.group_type, GroupType::Group);
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn test_get_by_mls_group_id_repairs_darkmatter_information_without_session_or_obsolete_mls_storage()
     {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member]).await;

        let group = {
            let creator_session = whitenoise.require_session(&creator.pubkey).unwrap();
            let config = GroupConfig::new(
                "darkmatter information without session".to_string(),
                "recreate group information from a persisted Darkmatter projection".to_string(),
                None,
                None,
                None,
                vec![RelayUrl::parse("ws://localhost:7777").unwrap()],
                vec![creator.pubkey],
                None,
            );
            creator_session
                .groups()
                .create_marmot_group_with_publisher(
                    vec![member.pubkey],
                    config,
                    None,
                    &RecordingMarmotPublisher,
                )
                .await
                .unwrap()
        };

        sqlx::query("DELETE FROM group_information WHERE mls_group_id = ?")
            .bind(group.mls_group_id.as_slice())
            .execute(&whitenoise.shared.database.pool)
            .await
            .unwrap();

        let removed_session = whitenoise
            .account_manager
            .remove_session(&creator.pubkey)
            .expect("creator session should exist");
        drop(removed_session);

        let obsolete_mls_storage_path = crate::whitenoise::test_utils::obsolete_mls_storage_path(
            &creator.pubkey,
            data_temp.path(),
        );
        if obsolete_mls_storage_path.is_dir() {
            fs::remove_dir_all(&obsolete_mls_storage_path).unwrap();
        } else if obsolete_mls_storage_path.exists() {
            fs::remove_file(&obsolete_mls_storage_path).unwrap();
        }
        assert!(!obsolete_mls_storage_path.exists());

        let group_info = whitenoise
            .get_group_information_by_mls_group_id(creator.pubkey, &group.mls_group_id)
            .await
            .unwrap();

        assert_eq!(group_info.mls_group_id, group.mls_group_id);
        assert_eq!(group_info.group_type, GroupType::Group);
        assert!(
            !obsolete_mls_storage_path.exists(),
            "Darkmatter group information repair must not recreate obsolete MLS storage"
        );
    }

    #[tokio::test]
    async fn test_public_group_information_ignores_unprojected_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let group_id = setup_unprojected_accepted_group(&whitenoise, &[&creator, &member]).await;

        sqlx::query("DELETE FROM group_information WHERE mls_group_id = ?")
            .bind(group_id.as_slice())
            .execute(&whitenoise.shared.database.pool)
            .await
            .unwrap();

        let single_result = whitenoise
            .get_group_information_by_mls_group_id(creator.pubkey, &group_id)
            .await;
        assert!(
            matches!(single_result, Err(WhitenoiseError::GroupNotFound)),
            "unprojected group information lookup must return GroupNotFound, got {single_result:?}"
        );

        let batch_result = whitenoise
            .get_group_information_by_mls_group_ids(creator.pubkey, std::slice::from_ref(&group_id))
            .await;
        assert!(
            matches!(batch_result, Err(WhitenoiseError::GroupNotFound)),
            "unprojected batch group information lookup must return GroupNotFound, got {batch_result:?}"
        );
    }

    #[tokio::test]
    async fn test_get_by_mls_group_id_missing_without_session_does_not_create_obsolete_mls_storage()
    {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = Account::new_external(&whitenoise, Keys::generate().public_key())
            .await
            .unwrap()
            .save(&whitenoise.shared.database)
            .await
            .unwrap();

        let obsolete_mls_storage_path = crate::whitenoise::test_utils::obsolete_mls_storage_path(
            &creator.pubkey,
            data_temp.path(),
        );
        if obsolete_mls_storage_path.is_dir() {
            fs::remove_dir_all(&obsolete_mls_storage_path).unwrap();
        } else if obsolete_mls_storage_path.exists() {
            fs::remove_file(&obsolete_mls_storage_path).unwrap();
        }
        assert!(!obsolete_mls_storage_path.exists());

        let missing_group_id = GroupId::from_slice(&[33; 32]);
        let result = whitenoise
            .get_group_information_by_mls_group_id(creator.pubkey, &missing_group_id)
            .await;

        assert!(
            matches!(result, Err(WhitenoiseError::GroupNotFound)),
            "expected GroupNotFound without creating obsolete MLS storage, got {result:?}"
        );
        assert!(
            !obsolete_mls_storage_path.exists(),
            "missing group lookups must not recreate obsolete MLS storage"
        );
    }

    #[tokio::test]
    async fn test_get_by_mls_group_ids_missing_without_session_does_not_create_obsolete_mls_storage()
     {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = Account::new_external(&whitenoise, Keys::generate().public_key())
            .await
            .unwrap()
            .save(&whitenoise.shared.database)
            .await
            .unwrap();

        let obsolete_mls_storage_path = crate::whitenoise::test_utils::obsolete_mls_storage_path(
            &creator.pubkey,
            data_temp.path(),
        );
        if obsolete_mls_storage_path.is_dir() {
            fs::remove_dir_all(&obsolete_mls_storage_path).unwrap();
        } else if obsolete_mls_storage_path.exists() {
            fs::remove_file(&obsolete_mls_storage_path).unwrap();
        }
        assert!(!obsolete_mls_storage_path.exists());

        let missing_group_id = GroupId::from_slice(&[34; 32]);
        let result = whitenoise
            .get_group_information_by_mls_group_ids(
                creator.pubkey,
                std::slice::from_ref(&missing_group_id),
            )
            .await;

        assert!(
            matches!(result, Err(WhitenoiseError::GroupNotFound)),
            "expected GroupNotFound without creating obsolete MLS storage, got {result:?}"
        );
        assert!(
            !obsolete_mls_storage_path.exists(),
            "missing batch group lookups must not recreate obsolete MLS storage"
        );
    }

    #[tokio::test]
    async fn test_get_by_mls_group_ids_repairs_darkmatter_group_information_without_obsolete_mls_storage()
     {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member]).await;

        let creator_session = whitenoise.require_session(&creator.pubkey).unwrap();
        let config = GroupConfig::new(
            "darkmatter batch information".to_string(),
            "recreate batch group information from Darkmatter projection".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("ws://localhost:7777").unwrap()],
            vec![creator.pubkey],
            None,
        );
        let group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member.pubkey],
                config,
                None,
                &RecordingMarmotPublisher,
            )
            .await
            .unwrap();
        let artifacts = remove_obsolete_mls_artifacts(&creator, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        sqlx::query("DELETE FROM group_information WHERE mls_group_id = ?")
            .bind(group.mls_group_id.as_slice())
            .execute(&whitenoise.shared.database.pool)
            .await
            .unwrap();

        let group_infos = whitenoise
            .get_group_information_by_mls_group_ids(
                creator.pubkey,
                std::slice::from_ref(&group.mls_group_id),
            )
            .await
            .unwrap();

        assert_eq!(group_infos.len(), 1);
        assert_eq!(group_infos[0].mls_group_id, group.mls_group_id);
        assert_eq!(group_infos[0].group_type, GroupType::Group);
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn test_get_by_mls_group_ids_all_existing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id1 = GroupId::from_slice(&[7; 32]);
        let group_id2 = GroupId::from_slice(&[8; 32]);

        // Create both groups first
        let _info1 = GroupInformation::create_for_group(
            &whitenoise,
            &group_id1.clone(),
            Some(GroupType::Group),
            "test",
        )
        .await
        .unwrap();

        let _info2 = GroupInformation::create_for_group(
            &whitenoise,
            &group_id2.clone(),
            Some(GroupType::DirectMessage),
            "",
        )
        .await
        .unwrap();

        let creator_account = whitenoise.create_identity().await.unwrap();

        // Get both
        let result = GroupInformation::get_by_mls_group_ids(
            creator_account.pubkey,
            &[group_id1.clone(), group_id2.clone()],
            &whitenoise,
        )
        .await;

        assert!(result.is_ok());
        let group_infos = result.unwrap();
        assert_eq!(group_infos.len(), 2);

        // Check that we got both groups with correct types
        let mut found_group = false;
        let mut found_dm = false;
        for info in &group_infos {
            if info.mls_group_id == group_id1 {
                assert_eq!(info.group_type, GroupType::Group);
                found_group = true;
            } else if info.mls_group_id == group_id2 {
                assert_eq!(info.group_type, GroupType::DirectMessage);
                found_dm = true;
            }
        }
        assert!(found_group && found_dm);
    }

    #[tokio::test]
    async fn test_get_by_mls_group_ids_mixed_existing_and_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member1 = whitenoise.create_identity().await.unwrap();
        let member2 = whitenoise.create_identity().await.unwrap();
        let member3 = whitenoise.create_identity().await.unwrap();

        // Create actual MLS groups
        let mut config1 = crate::whitenoise::test_utils::create_group_config(vec![
            creator_account.pubkey,
            member1.pubkey,
        ]);
        config1.name = "".to_string(); // Empty name for DirectMessage
        let group1 = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member1.pubkey], config1, None)
            .await
            .unwrap();

        let config2 =
            crate::whitenoise::test_utils::create_group_config(vec![creator_account.pubkey]);
        let group2 = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member2.pubkey], config2, None)
            .await
            .unwrap();

        let config3 =
            crate::whitenoise::test_utils::create_group_config(vec![creator_account.pubkey]);
        let group3 = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member3.pubkey], config3, None)
            .await
            .unwrap();

        // Create only the first one in database via create_for_group
        let _info1 = GroupInformation::create_for_group(
            &whitenoise,
            &group1.mls_group_id,
            Some(GroupType::DirectMessage),
            "",
        )
        .await
        .unwrap();

        // Get all three (one exists in db, two missing from db)
        let result = GroupInformation::get_by_mls_group_ids(
            creator_account.pubkey,
            &[
                group1.mls_group_id.clone(),
                group2.mls_group_id.clone(),
                group3.mls_group_id.clone(),
            ],
            &whitenoise,
        )
        .await;

        assert!(result.is_ok());
        let group_infos = result.unwrap();
        assert_eq!(group_infos.len(), 3);

        // Check results - should preserve order from input
        assert_eq!(group_infos[0].mls_group_id, group1.mls_group_id);
        assert_eq!(group_infos[0].group_type, GroupType::DirectMessage); // Existing type preserved

        assert_eq!(group_infos[1].mls_group_id, group2.mls_group_id);
        assert_eq!(group_infos[1].group_type, GroupType::Group); // Default for new

        assert_eq!(group_infos[2].mls_group_id, group3.mls_group_id);
        assert_eq!(group_infos[2].group_type, GroupType::Group); // Default for new
    }

    #[tokio::test]
    async fn test_get_by_mls_group_ids_all_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member1 = whitenoise.create_identity().await.unwrap();
        let member2 = whitenoise.create_identity().await.unwrap();

        // Create actual MLS groups
        let config1 =
            crate::whitenoise::test_utils::create_group_config(vec![creator_account.pubkey]);
        let group1 = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member1.pubkey], config1, None)
            .await
            .unwrap();

        let config2 =
            crate::whitenoise::test_utils::create_group_config(vec![creator_account.pubkey]);
        let group2 = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member2.pubkey], config2, None)
            .await
            .unwrap();

        // Get both (neither exists in database yet)
        let result = GroupInformation::get_by_mls_group_ids(
            creator_account.pubkey,
            &[group1.mls_group_id.clone(), group2.mls_group_id.clone()],
            &whitenoise,
        )
        .await;

        assert!(result.is_ok());
        let group_infos = result.unwrap();
        assert_eq!(group_infos.len(), 2);

        // Both should be created with default type
        assert_eq!(group_infos[0].mls_group_id, group1.mls_group_id);
        assert_eq!(group_infos[0].group_type, GroupType::Group);

        assert_eq!(group_infos[1].mls_group_id, group2.mls_group_id);
        assert_eq!(group_infos[1].group_type, GroupType::Group);
    }

    #[tokio::test]
    async fn test_get_by_mls_group_ids_empty_input() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();

        let result =
            GroupInformation::get_by_mls_group_ids(creator_account.pubkey, &[], &whitenoise).await;

        assert!(result.is_ok());
        let group_infos = result.unwrap();
        assert!(group_infos.is_empty());
    }

    #[tokio::test]
    async fn test_get_by_mls_group_ids_preserves_order() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member1 = whitenoise.create_identity().await.unwrap();
        let member2 = whitenoise.create_identity().await.unwrap();
        let member3 = whitenoise.create_identity().await.unwrap();

        // Create actual MLS groups
        let config1 =
            crate::whitenoise::test_utils::create_group_config(vec![creator_account.pubkey]);
        let group1 = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member1.pubkey], config1, None)
            .await
            .unwrap();

        let config2 =
            crate::whitenoise::test_utils::create_group_config(vec![creator_account.pubkey]);
        let group2 = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member2.pubkey], config2, None)
            .await
            .unwrap();

        let config3 =
            crate::whitenoise::test_utils::create_group_config(vec![creator_account.pubkey]);
        let group3 = whitenoise
            .require_session(&creator_account.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member3.pubkey], config3, None)
            .await
            .unwrap();

        // Test order preservation when all are missing from database
        let result = GroupInformation::get_by_mls_group_ids(
            creator_account.pubkey,
            &[
                group2.mls_group_id.clone(),
                group1.mls_group_id.clone(),
                group3.mls_group_id.clone(),
            ], // Intentional different order
            &whitenoise,
        )
        .await;

        assert!(result.is_ok());
        let group_infos = result.unwrap();
        assert_eq!(group_infos.len(), 3);

        // Should preserve input order
        assert_eq!(group_infos[0].mls_group_id, group2.mls_group_id);
        assert_eq!(group_infos[1].mls_group_id, group1.mls_group_id);
        assert_eq!(group_infos[2].mls_group_id, group3.mls_group_id);
    }
}

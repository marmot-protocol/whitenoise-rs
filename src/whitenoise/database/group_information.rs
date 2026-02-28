use std::str::FromStr;

use chrono::{DateTime, Utc};
use mdk_core::prelude::GroupId;

use super::{Database, utils::parse_timestamp};
use crate::whitenoise::{
    error::WhitenoiseError,
    group_information::{GroupInformation, GroupType},
};

/// Internal database row representation for group_information table
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
struct GroupInformationRow {
    id: i64,
    mls_group_id: GroupId,
    group_type: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl<'r, R> sqlx::FromRow<'r, R> for GroupInformationRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Vec<u8>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let mls_group_id_bytes: Vec<u8> = row.try_get("mls_group_id")?;
        let group_type: String = row.try_get("group_type")?;

        let mls_group_id = GroupId::from_slice(&mls_group_id_bytes);
        let created_at = parse_timestamp(row, "created_at")?;
        let updated_at = parse_timestamp(row, "updated_at")?;

        Ok(Self {
            id,
            mls_group_id,
            group_type,
            created_at,
            updated_at,
        })
    }
}

impl GroupInformationRow {
    fn into_group_information(self) -> Result<GroupInformation, WhitenoiseError> {
        let group_type = GroupType::from_str(&self.group_type).map_err(|e| {
            WhitenoiseError::Configuration(format!(
                "Invalid group type '{}': {}",
                self.group_type, e
            ))
        })?;

        Ok(GroupInformation {
            id: Some(self.id),
            mls_group_id: self.mls_group_id,
            group_type,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

impl GroupInformation {
    /// Finds a GroupInformation by MLS group ID.
    ///
    /// # Arguments
    ///
    /// * `mls_group_id` - The MLS group ID to search for
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns the `GroupInformation` associated with the provided MLS group ID on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if no group information with the given MLS group ID exists.
    pub(crate) async fn find_by_mls_group_id(
        mls_group_id: &GroupId,
        database: &Database,
    ) -> Result<Self, WhitenoiseError> {
        let group_information_row = sqlx::query_as::<_, GroupInformationRow>(
            "SELECT id, mls_group_id, group_type, created_at, updated_at FROM group_information WHERE mls_group_id = ?",
        )
        .bind(mls_group_id.as_slice())
        .fetch_one(&database.pool)
        .await?;

        group_information_row.into_group_information()
    }

    /// Finds an existing GroupInformation by MLS group ID or creates a new one if not found.
    ///
    /// # Arguments
    ///
    /// * `mls_group_id` - The MLS group ID to search for
    /// * `group_type` - The group type to use when creating a new record
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns a tuple containing the `GroupInformation` and a boolean indicating if it was newly created (true) or found (false).
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database operations fail.
    pub(crate) async fn find_or_create_by_mls_group_id(
        mls_group_id: &GroupId,
        group_type: Option<GroupType>,
        database: &Database,
    ) -> Result<(Self, bool), WhitenoiseError> {
        match Self::find_by_mls_group_id(mls_group_id, database).await {
            Ok(group_info) => Ok((group_info, false)),
            Err(WhitenoiseError::SqlxError(sqlx::Error::RowNotFound)) => {
                let group_info =
                    Self::insert_new(mls_group_id, group_type.unwrap_or_default(), database)
                        .await?;
                Ok((group_info, true))
            }
            Err(e) => Err(e),
        }
    }

    /// Finds multiple GroupInformation records by MLS group IDs.
    ///
    /// # Arguments
    ///
    /// * `mls_group_ids` - A slice of MLS group IDs to search for
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns a `Vec<GroupInformation>` containing only the records that exist.
    /// Missing records are not included in the result.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database query fails.
    pub(crate) async fn find_by_mls_group_ids(
        mls_group_ids: &[GroupId],
        database: &Database,
    ) -> Result<Vec<Self>, WhitenoiseError> {
        if mls_group_ids.is_empty() {
            return Ok(Vec::new());
        }

        let id_bytes: Vec<Vec<u8>> = mls_group_ids.iter().map(|id| id.to_vec()).collect();

        let mut qb: sqlx::QueryBuilder<sqlx::Sqlite> = sqlx::QueryBuilder::new(
            "SELECT id, mls_group_id, group_type, created_at, updated_at FROM group_information WHERE mls_group_id IN (",
        );
        let mut sep = qb.separated(", ");
        for id in &id_bytes {
            sep.push_bind(id);
        }
        sep.push_unseparated(")");

        let rows = qb
            .build_query_as::<GroupInformationRow>()
            .fetch_all(&database.pool)
            .await?;

        rows.into_iter()
            .map(|row| row.into_group_information())
            .collect::<Result<Vec<_>, _>>()
    }

    // Private helper method for creating and persisting new records
    async fn insert_new(
        mls_group_id: &GroupId,
        group_type: GroupType,
        database: &Database,
    ) -> Result<Self, WhitenoiseError> {
        let now_ms = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, GroupInformationRow>(
            "INSERT INTO group_information (mls_group_id, group_type, created_at, updated_at)
             VALUES (?, ?, ?, ?)
             RETURNING id, mls_group_id, group_type, created_at, updated_at",
        )
        .bind(mls_group_id.as_slice())
        .bind(group_type.to_string())
        .bind(now_ms)
        .bind(now_ms)
        .fetch_one(&database.pool)
        .await?;

        row.into_group_information()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::test_utils::create_mock_whitenoise;
    use mdk_core::prelude::GroupId;

    #[tokio::test]
    async fn test_find_by_mls_group_id_not_found() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[1; 32]);

        let result = GroupInformation::find_by_mls_group_id(&group_id, &whitenoise.database).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WhitenoiseError::SqlxError(sqlx::Error::RowNotFound)
        ));
    }

    #[tokio::test]
    async fn test_find_or_create_by_mls_group_id_creates_new() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[2; 32]);

        let (group_info, was_created) = GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert!(was_created);
        assert_eq!(group_info.mls_group_id, group_id);
        assert_eq!(group_info.group_type, GroupType::Group);
        assert!(group_info.id.is_some());
    }

    #[tokio::test]
    async fn test_find_or_create_by_mls_group_id_finds_existing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[3; 32]);

        // First create
        let (original_group_info, was_created) = GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::DirectMessage),
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(was_created);

        // Second call should find existing
        let (found_group_info, was_created) = GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group), // Different type, but should find existing
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert!(!was_created);
        assert_eq!(found_group_info.id, original_group_info.id);
        assert_eq!(found_group_info.mls_group_id, group_id);
        assert_eq!(found_group_info.group_type, GroupType::DirectMessage); // Original type preserved
    }

    #[tokio::test]
    async fn test_find_or_create_with_default_group_type() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[4; 32]);

        let (group_info, was_created) = GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            None, // Should use default (Group)
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert!(was_created);
        assert_eq!(group_info.group_type, GroupType::Group);
    }

    #[tokio::test]
    async fn test_find_by_mls_group_ids_empty_input() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let result = GroupInformation::find_by_mls_group_ids(&[], &whitenoise.database)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_find_by_mls_group_ids_single_existing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[5; 32]);

        // Create a group first
        let (created_group_info, _) = GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::DirectMessage),
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Find it in batch query
        let results = GroupInformation::find_by_mls_group_ids(
            std::slice::from_ref(&group_id),
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, created_group_info.id);
        assert_eq!(results[0].mls_group_id, group_id);
        assert_eq!(results[0].group_type, GroupType::DirectMessage);
    }

    #[tokio::test]
    async fn test_find_by_mls_group_ids_multiple_mixed() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id1 = GroupId::from_slice(&[6; 32]);
        let group_id2 = GroupId::from_slice(&[7; 32]);
        let group_id3 = GroupId::from_slice(&[8; 32]); // This one won't be created

        // Create two groups
        let (group_info1, _) = GroupInformation::find_or_create_by_mls_group_id(
            &group_id1,
            Some(GroupType::Group),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let (group_info2, _) = GroupInformation::find_or_create_by_mls_group_id(
            &group_id2,
            Some(GroupType::DirectMessage),
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Query for all three (one missing)
        let results = GroupInformation::find_by_mls_group_ids(
            &[group_id1.clone(), group_id2.clone(), group_id3],
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Should only return the two that exist
        assert_eq!(results.len(), 2);

        // Results might be in any order, so check both exist
        let ids: Vec<_> = results.iter().map(|gi| gi.id).collect();
        assert!(ids.contains(&group_info1.id));
        assert!(ids.contains(&group_info2.id));

        // Verify group types are correct
        for result in &results {
            if result.mls_group_id == group_id1 {
                assert_eq!(result.group_type, GroupType::Group);
            } else if result.mls_group_id == group_id2 {
                assert_eq!(result.group_type, GroupType::DirectMessage);
            }
        }
    }

    #[tokio::test]
    async fn test_find_by_mls_group_ids_none_exist() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id1 = GroupId::from_slice(&[9; 32]);
        let group_id2 = GroupId::from_slice(&[10; 32]);

        let results =
            GroupInformation::find_by_mls_group_ids(&[group_id1, group_id2], &whitenoise.database)
                .await
                .unwrap();

        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_group_type_conversion() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[11; 32]);

        // Test DirectMessage type
        let (dm_group_info, _) = GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::DirectMessage),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let found_dm = GroupInformation::find_by_mls_group_id(&group_id, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(found_dm.group_type, GroupType::DirectMessage);
        assert_eq!(found_dm.id, dm_group_info.id);
    }
}

use chrono::{DateTime, Utc};
use mdk_core::prelude::GroupId;
use nostr_sdk::PublicKey;

use super::{Database, utils::parse_timestamp};
use crate::whitenoise::accounts_groups::AccountGroup;

/// Internal database row representation for accounts_groups table
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
struct AccountGroupRow {
    id: i64,
    account_pubkey: PublicKey,
    mls_group_id: GroupId,
    user_confirmation: Option<bool>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl<'r, R> sqlx::FromRow<'r, R> for AccountGroupRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Vec<u8>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Option<i64>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let account_pubkey_str: String = row.try_get("account_pubkey")?;
        let mls_group_id_bytes: Vec<u8> = row.try_get("mls_group_id")?;
        let user_confirmation_int: Option<i64> = row.try_get("user_confirmation")?;

        // Parse pubkey from hex string
        let account_pubkey =
            PublicKey::parse(&account_pubkey_str).map_err(|e| sqlx::Error::ColumnDecode {
                index: "account_pubkey".to_string(),
                source: Box::new(e),
            })?;

        let mls_group_id = GroupId::from_slice(&mls_group_id_bytes);

        // Validate user_confirmation: only 0, 1, or NULL are valid
        let user_confirmation = match user_confirmation_int {
            None => None,
            Some(0) => Some(false),
            Some(1) => Some(true),
            Some(v) => {
                return Err(sqlx::Error::ColumnDecode {
                    index: "user_confirmation".to_string(),
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "Invalid user_confirmation value: expected 0, 1, or NULL, got {}",
                            v
                        ),
                    )),
                });
            }
        };

        let created_at = parse_timestamp(row, "created_at")?;
        let updated_at = parse_timestamp(row, "updated_at")?;

        Ok(Self {
            id,
            account_pubkey,
            mls_group_id,
            user_confirmation,
            created_at,
            updated_at,
        })
    }
}

impl From<AccountGroupRow> for AccountGroup {
    fn from(row: AccountGroupRow) -> Self {
        AccountGroup {
            id: Some(row.id),
            account_pubkey: row.account_pubkey,
            mls_group_id: row.mls_group_id,
            user_confirmation: row.user_confirmation,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

impl AccountGroup {
    /// Finds an AccountGroup by account pubkey and MLS group ID.
    pub(crate) async fn find_by_account_and_group(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        database: &Database,
    ) -> Result<Option<Self>, sqlx::Error> {
        let row = sqlx::query_as::<_, AccountGroupRow>(
            "SELECT id, account_pubkey, mls_group_id, user_confirmation, created_at, updated_at
             FROM accounts_groups
             WHERE account_pubkey = ? AND mls_group_id = ?",
        )
        .bind(account_pubkey.to_hex())
        .bind(mls_group_id.as_slice())
        .fetch_optional(&database.pool)
        .await?;

        match row {
            Some(r) => Ok(Some(r.into())),
            None => Ok(None),
        }
    }

    /// Finds or creates an AccountGroup for the given account and group.
    /// Returns the AccountGroup and a boolean indicating if it was newly created.
    ///
    /// This uses an insert-first approach to avoid TOCTOU race conditions:
    /// - Attempts to insert first
    /// - On unique constraint violation, fetches the existing record
    pub(crate) async fn find_or_create(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        database: &Database,
    ) -> Result<(Self, bool), sqlx::Error> {
        // Try to create first - this handles the race condition properly
        match Self::create(account_pubkey, mls_group_id, database).await {
            Ok(created) => Ok((created, true)),
            Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
                // Another task inserted the record between our check and insert
                // Fetch the existing record
                let existing =
                    Self::find_by_account_and_group(account_pubkey, mls_group_id, database)
                        .await?
                        .ok_or(sqlx::Error::RowNotFound)?;
                Ok((existing, false))
            }
            Err(e) => Err(e),
        }
    }

    /// Finds all visible AccountGroups for a given account.
    /// Visible means: user_confirmation is NULL (pending) or true (accepted).
    /// Declined groups (user_confirmation = false) are hidden.
    pub(crate) async fn find_visible_for_account(
        account_pubkey: &PublicKey,
        database: &Database,
    ) -> Result<Vec<Self>, sqlx::Error> {
        let rows = sqlx::query_as::<_, AccountGroupRow>(
            "SELECT id, account_pubkey, mls_group_id, user_confirmation, created_at, updated_at
             FROM accounts_groups
             WHERE account_pubkey = ? AND (user_confirmation IS NULL OR user_confirmation = TRUE)",
        )
        .bind(account_pubkey.to_hex())
        .fetch_all(&database.pool)
        .await?;

        Ok(rows.into_iter().map(|r| r.into()).collect())
    }

    /// Finds all pending AccountGroups for a given account.
    /// Pending means: user_confirmation is NULL.
    pub(crate) async fn find_pending_for_account(
        account_pubkey: &PublicKey,
        database: &Database,
    ) -> Result<Vec<Self>, sqlx::Error> {
        let rows = sqlx::query_as::<_, AccountGroupRow>(
            "SELECT id, account_pubkey, mls_group_id, user_confirmation, created_at, updated_at
             FROM accounts_groups
             WHERE account_pubkey = ? AND user_confirmation IS NULL",
        )
        .bind(account_pubkey.to_hex())
        .fetch_all(&database.pool)
        .await?;

        Ok(rows.into_iter().map(|r| r.into()).collect())
    }

    /// Updates the user_confirmation status for this AccountGroup.
    pub(crate) async fn update_user_confirmation(
        &self,
        user_confirmation: bool,
        database: &Database,
    ) -> Result<Self, sqlx::Error> {
        let id = self.id.ok_or_else(|| {
            sqlx::Error::Protocol("Cannot update unsaved AccountGroup".to_string())
        })?;

        let now_ms = Utc::now().timestamp_millis();
        let confirmation_int: i64 = if user_confirmation { 1 } else { 0 };

        let row = sqlx::query_as::<_, AccountGroupRow>(
            "UPDATE accounts_groups
             SET user_confirmation = ?, updated_at = ?
             WHERE id = ?
             RETURNING id, account_pubkey, mls_group_id, user_confirmation, created_at, updated_at",
        )
        .bind(confirmation_int)
        .bind(now_ms)
        .bind(id)
        .fetch_one(&database.pool)
        .await?;

        Ok(row.into())
    }

    /// Deletes this AccountGroup from the database.
    pub(crate) async fn delete(&self, database: &Database) -> Result<(), sqlx::Error> {
        let id = self.id.ok_or_else(|| {
            sqlx::Error::Protocol("Cannot delete unsaved AccountGroup".to_string())
        })?;

        sqlx::query("DELETE FROM accounts_groups WHERE id = ?")
            .bind(id)
            .execute(&database.pool)
            .await?;

        Ok(())
    }

    /// Creates a new AccountGroup with user_confirmation = NULL (pending).
    async fn create(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        database: &Database,
    ) -> Result<Self, sqlx::Error> {
        let now_ms = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, AccountGroupRow>(
            "INSERT INTO accounts_groups (account_pubkey, mls_group_id, user_confirmation, created_at, updated_at)
             VALUES (?, ?, NULL, ?, ?)
             RETURNING id, account_pubkey, mls_group_id, user_confirmation, created_at, updated_at",
        )
        .bind(account_pubkey.to_hex())
        .bind(mls_group_id.as_slice())
        .bind(now_ms)
        .bind(now_ms)
        .fetch_one(&database.pool)
        .await?;

        Ok(row.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    #[tokio::test]
    async fn test_find_by_account_and_group_not_found() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[1; 32]);

        let result = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_find_or_create_creates_new() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[2; 32]);

        let (account_group, was_created) =
            AccountGroup::find_or_create(&account.pubkey, &group_id, &whitenoise.database)
                .await
                .unwrap();

        assert!(was_created);
        assert_eq!(account_group.account_pubkey, account.pubkey);
        assert_eq!(account_group.mls_group_id, group_id);
        assert!(account_group.user_confirmation.is_none()); // Should be pending
        assert!(account_group.id.is_some());
    }

    #[tokio::test]
    async fn test_find_or_create_finds_existing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[3; 32]);

        // First create
        let (original, was_created) =
            AccountGroup::find_or_create(&account.pubkey, &group_id, &whitenoise.database)
                .await
                .unwrap();
        assert!(was_created);

        // Second call should find existing
        let (found, was_created) =
            AccountGroup::find_or_create(&account.pubkey, &group_id, &whitenoise.database)
                .await
                .unwrap();

        assert!(!was_created);
        assert_eq!(found.id, original.id);
    }

    #[tokio::test]
    async fn test_update_user_confirmation_accept() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[4; 32]);

        let (account_group, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id, &whitenoise.database)
                .await
                .unwrap();

        assert!(account_group.user_confirmation.is_none());

        let updated = account_group
            .update_user_confirmation(true, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(updated.user_confirmation, Some(true));
        assert_eq!(updated.id, account_group.id);
    }

    #[tokio::test]
    async fn test_update_user_confirmation_decline() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[5; 32]);

        let (account_group, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id, &whitenoise.database)
                .await
                .unwrap();

        let updated = account_group
            .update_user_confirmation(false, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(updated.user_confirmation, Some(false));
    }

    #[tokio::test]
    async fn test_find_visible_for_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id1 = GroupId::from_slice(&[8; 32]); // Will be pending
        let group_id2 = GroupId::from_slice(&[9; 32]); // Will be accepted
        let group_id3 = GroupId::from_slice(&[10; 32]); // Will be declined

        let (_ag1, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id1, &whitenoise.database)
                .await
                .unwrap();
        let (ag2, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id2, &whitenoise.database)
                .await
                .unwrap();
        let (ag3, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id3, &whitenoise.database)
                .await
                .unwrap();

        // ag1 stays pending (NULL)
        ag2.update_user_confirmation(true, &whitenoise.database)
            .await
            .unwrap();
        ag3.update_user_confirmation(false, &whitenoise.database)
            .await
            .unwrap();

        let visible = AccountGroup::find_visible_for_account(&account.pubkey, &whitenoise.database)
            .await
            .unwrap();

        // Should only include pending and accepted, not declined
        assert_eq!(visible.len(), 2);
        let ids: Vec<_> = visible.iter().map(|ag| ag.mls_group_id.clone()).collect();
        assert!(ids.contains(&group_id1)); // pending
        assert!(ids.contains(&group_id2)); // accepted
        assert!(!ids.contains(&group_id3)); // declined - should NOT be visible
    }

    #[tokio::test]
    async fn test_find_pending_for_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id1 = GroupId::from_slice(&[11; 32]); // Will be pending
        let group_id2 = GroupId::from_slice(&[12; 32]); // Will be accepted

        let (_, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id1, &whitenoise.database)
                .await
                .unwrap();
        let (ag2, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id2, &whitenoise.database)
                .await
                .unwrap();

        ag2.update_user_confirmation(true, &whitenoise.database)
            .await
            .unwrap();

        let pending = AccountGroup::find_pending_for_account(&account.pubkey, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].mls_group_id, group_id1);
    }

    #[tokio::test]
    async fn test_delete() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[13; 32]);

        let (account_group, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id, &whitenoise.database)
                .await
                .unwrap();

        account_group.delete(&whitenoise.database).await.unwrap();

        let result = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_different_accounts_same_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account1 = whitenoise.create_identity().await.unwrap();
        let account2 = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[14; 32]);

        let (ag1, created1) =
            AccountGroup::find_or_create(&account1.pubkey, &group_id, &whitenoise.database)
                .await
                .unwrap();
        let (ag2, created2) =
            AccountGroup::find_or_create(&account2.pubkey, &group_id, &whitenoise.database)
                .await
                .unwrap();

        assert!(created1);
        assert!(created2);
        assert_ne!(ag1.id, ag2.id);
        assert_eq!(ag1.mls_group_id, ag2.mls_group_id);
        assert_ne!(ag1.account_pubkey, ag2.account_pubkey);
    }
}

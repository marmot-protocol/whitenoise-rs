use chrono::{DateTime, Utc};
use mdk_core::prelude::GroupId;
use nostr_sdk::prelude::*;

use super::{Database, utils::parse_timestamp};
use crate::whitenoise::accounts_groups::AccountGroup;

/// Internal database row representation for accounts_groups table
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
struct AccountGroupRow {
    id: i64,
    account_pubkey: PublicKey,
    mls_group_id: GroupId,
    user_confirmation: Option<bool>,
    welcomer_pubkey: Option<PublicKey>,
    last_read_message_id: Option<EventId>,
    pin_order: Option<i64>,
    dm_peer_pubkey: Option<PublicKey>,
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
        let welcomer_pubkey_str: Option<String> = row.try_get("welcomer_pubkey")?;
        let welcomer_pubkey = match welcomer_pubkey_str {
            Some(s) => Some(PublicKey::parse(&s).map_err(|e| sqlx::Error::ColumnDecode {
                index: "welcomer_pubkey".to_string(),
                source: Box::new(e),
            })?),
            None => None,
        };

        let last_read_message_id_str: Option<String> = row.try_get("last_read_message_id")?;
        let last_read_message_id = match last_read_message_id_str {
            Some(hex) => Some(
                EventId::from_hex(&hex).map_err(|e| sqlx::Error::ColumnDecode {
                    index: "last_read_message_id".to_string(),
                    source: Box::new(e),
                })?,
            ),
            None => None,
        };

        let pin_order: Option<i64> = row.try_get("pin_order")?;

        let dm_peer_pubkey_str: Option<String> = row.try_get("dm_peer_pubkey")?;
        let dm_peer_pubkey = match dm_peer_pubkey_str {
            Some(s) => Some(PublicKey::parse(&s).map_err(|e| sqlx::Error::ColumnDecode {
                index: "dm_peer_pubkey".to_string(),
                source: Box::new(e),
            })?),
            None => None,
        };

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
            welcomer_pubkey,
            last_read_message_id,
            pin_order,
            dm_peer_pubkey,
            created_at,
            updated_at,
        })
    }
}

impl From<AccountGroupRow> for AccountGroup {
    fn from(row: AccountGroupRow) -> Self {
        Self {
            id: Some(row.id),
            account_pubkey: row.account_pubkey,
            mls_group_id: row.mls_group_id,
            user_confirmation: row.user_confirmation,
            welcomer_pubkey: row.welcomer_pubkey,
            last_read_message_id: row.last_read_message_id,
            pin_order: row.pin_order,
            dm_peer_pubkey: row.dm_peer_pubkey,
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
            "SELECT *
             FROM accounts_groups
             WHERE account_pubkey = ? AND mls_group_id = ?",
        )
        .bind(account_pubkey.to_hex())
        .bind(mls_group_id.as_slice())
        .fetch_optional(&database.pool)
        .await?;

        Ok(row.map(Into::into))
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
        dm_peer_pubkey: Option<&PublicKey>,
        database: &Database,
    ) -> Result<(Self, bool), sqlx::Error> {
        // Try to create first - this handles the race condition properly
        match Self::create(account_pubkey, mls_group_id, None, dm_peer_pubkey, database).await {
            Ok(created) => Ok((created, true)),
            Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
                // Insert failed due to unique constraint - a concurrent task already created the row
                // Fall back to fetching the existing record
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
            "SELECT *
             FROM accounts_groups
             WHERE account_pubkey = ? AND (user_confirmation IS NULL OR user_confirmation = 1)",
        )
        .bind(account_pubkey.to_hex())
        .fetch_all(&database.pool)
        .await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Finds all pending AccountGroups for a given account.
    /// Pending means: user_confirmation is NULL.
    pub(crate) async fn find_pending_for_account(
        account_pubkey: &PublicKey,
        database: &Database,
    ) -> Result<Vec<Self>, sqlx::Error> {
        let rows = sqlx::query_as::<_, AccountGroupRow>(
            "SELECT *
             FROM accounts_groups
             WHERE account_pubkey = ? AND user_confirmation IS NULL",
        )
        .bind(account_pubkey.to_hex())
        .fetch_all(&database.pool)
        .await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Finds all AccountGroups for a specific MLS group.
    pub(crate) async fn find_by_group(
        mls_group_id: &GroupId,
        database: &Database,
    ) -> Result<Vec<Self>, sqlx::Error> {
        let rows = sqlx::query_as::<_, AccountGroupRow>(
            "SELECT *
             FROM accounts_groups
             WHERE mls_group_id = ?",
        )
        .bind(mls_group_id.as_slice())
        .fetch_all(&database.pool)
        .await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Updates the user_confirmation status for this AccountGroup.
    pub(crate) async fn update_user_confirmation(
        &self,
        user_confirmation: bool,
        database: &Database,
    ) -> Result<Self, sqlx::Error> {
        let id = self.id.expect("AccountGroup must be persisted");

        let now_ms = Utc::now().timestamp_millis();
        let confirmation_int: i64 = if user_confirmation { 1 } else { 0 };

        let row = sqlx::query_as::<_, AccountGroupRow>(
            "UPDATE accounts_groups
             SET user_confirmation = ?, updated_at = ?
             WHERE id = ?
             RETURNING *",
        )
        .bind(confirmation_int)
        .bind(now_ms)
        .bind(id)
        .fetch_one(&database.pool)
        .await?;

        Ok(row.into())
    }

    /// Saves the AccountGroup to the database (upsert).
    ///
    /// - If the record doesn't exist, inserts it
    /// - If it exists, updates all mutable fields to match the provided values
    pub(crate) async fn save(&self, database: &Database) -> Result<Self, sqlx::Error> {
        let now_ms = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, AccountGroupRow>(
            "INSERT INTO accounts_groups (account_pubkey, mls_group_id, user_confirmation, welcomer_pubkey, last_read_message_id, pin_order, dm_peer_pubkey, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(account_pubkey, mls_group_id) DO UPDATE SET
               user_confirmation = excluded.user_confirmation,
               welcomer_pubkey = excluded.welcomer_pubkey,
               last_read_message_id = excluded.last_read_message_id,
               pin_order = excluded.pin_order,
               -- Write-once: preserve existing dm_peer_pubkey if already set.
               -- Many code paths construct AccountGroup without knowing the DM peer
               -- (e.g. lazy migration), so we only fill this on first write and
               -- never overwrite a correct value with NULL.
               dm_peer_pubkey = COALESCE(accounts_groups.dm_peer_pubkey, excluded.dm_peer_pubkey),
               updated_at = excluded.updated_at
             RETURNING *",
        )
        .bind(self.account_pubkey.to_hex())
        .bind(self.mls_group_id.as_slice())
        .bind(self.user_confirmation.map(|b| if b { 1i64 } else { 0i64 }))
        .bind(self.welcomer_pubkey.as_ref().map(|pk| pk.to_hex()))
        .bind(self.last_read_message_id.as_ref().map(|id| id.to_hex()))
        .bind(self.pin_order)
        .bind(self.dm_peer_pubkey.as_ref().map(|pk| pk.to_hex()))
        .bind(self.created_at.timestamp_millis())
        .bind(now_ms)
        .fetch_one(&database.pool)
        .await?;

        Ok(row.into())
    }

    /// Updates the pin_order for this AccountGroup.
    pub(crate) async fn update_pin_order(
        &self,
        pin_order: Option<i64>,
        database: &Database,
    ) -> Result<Self, sqlx::Error> {
        let id = self.id.expect("AccountGroup must be persisted");
        let now_ms = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, AccountGroupRow>(
            "UPDATE accounts_groups
             SET pin_order = ?, updated_at = ?
             WHERE id = ?
             RETURNING *",
        )
        .bind(pin_order)
        .bind(now_ms)
        .bind(id)
        .fetch_one(&database.pool)
        .await?;

        Ok(row.into())
    }

    /// Atomically updates last_read_message_id only if the new message is newer.
    ///
    /// Returns `Some(updated)` if the update was applied, `None` if skipped
    /// because the new message is not newer than the current read marker.
    ///
    /// This is atomic: the timestamp comparison and update happen in a single
    /// SQL statement, preventing race conditions between concurrent calls.
    pub(crate) async fn update_last_read_if_newer(
        &self,
        message_id: &EventId,
        message_created_at_ms: i64,
        database: &Database,
    ) -> Result<Option<Self>, sqlx::Error> {
        let id = self.id.expect("AccountGroup must be persisted");
        let now_ms = Utc::now().timestamp_millis();

        // Atomic compare-and-update: only update if the new message is newer
        // than the current read marker. Uses a subquery to get the current
        // marker's timestamp from aggregated_messages, scoped to the same group.
        let row = sqlx::query_as::<_, AccountGroupRow>(
            "UPDATE accounts_groups
             SET last_read_message_id = ?, updated_at = ?
             WHERE id = ?
               AND (
                 last_read_message_id IS NULL
                 OR ? > COALESCE(
                   (SELECT created_at FROM aggregated_messages
                    WHERE message_id = accounts_groups.last_read_message_id
                      AND mls_group_id = accounts_groups.mls_group_id),
                   0
                 )
               )
             RETURNING *",
        )
        .bind(message_id.to_hex())
        .bind(now_ms)
        .bind(id)
        .bind(message_created_at_ms)
        .fetch_optional(&database.pool)
        .await?;

        Ok(row.map(Into::into))
    }

    /// Finds the most recently created visible DM group between an account and a peer.
    ///
    /// Uses the `dm_peer_pubkey` column for an efficient single-query lookup
    /// without requiring MLS/MDK calls. Returns `None` if no DM group exists
    /// between these users, or if the group has been declined.
    pub(crate) async fn find_dm_group_id_by_peer(
        account_pubkey: &PublicKey,
        peer_pubkey: &PublicKey,
        database: &Database,
    ) -> Result<Option<GroupId>, sqlx::Error> {
        let row: Option<(Vec<u8>,)> = sqlx::query_as(
            "SELECT ag.mls_group_id
             FROM accounts_groups ag
             WHERE ag.account_pubkey = ?
               AND ag.dm_peer_pubkey = ?
               AND (ag.user_confirmation IS NULL OR ag.user_confirmation = 1)
             ORDER BY ag.created_at DESC
             LIMIT 1",
        )
        .bind(account_pubkey.to_hex())
        .bind(peer_pubkey.to_hex())
        .fetch_optional(&database.pool)
        .await?;

        Ok(row.map(|(bytes,)| GroupId::from_slice(&bytes)))
    }

    /// Returns DM peer pubkeys for all visible DM groups belonging to an account.
    ///
    /// Returns `(mls_group_id, dm_peer_pubkey)` pairs for groups where
    /// `dm_peer_pubkey` is populated and the group is visible (not declined).
    pub(crate) async fn find_dm_peers_for_account(
        account_pubkey: &PublicKey,
        database: &Database,
    ) -> Result<Vec<(GroupId, PublicKey)>, sqlx::Error> {
        let rows: Vec<(Vec<u8>, String)> = sqlx::query_as(
            "SELECT ag.mls_group_id, ag.dm_peer_pubkey
             FROM accounts_groups ag
             WHERE ag.account_pubkey = ?
               AND ag.dm_peer_pubkey IS NOT NULL
               AND (ag.user_confirmation IS NULL OR ag.user_confirmation = 1)",
        )
        .bind(account_pubkey.to_hex())
        .fetch_all(&database.pool)
        .await?;

        let mut results = Vec::with_capacity(rows.len());
        for (group_id_bytes, peer_hex) in rows {
            let group_id = GroupId::from_slice(&group_id_bytes);
            match PublicKey::parse(&peer_hex) {
                Ok(pk) => results.push((group_id, pk)),
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::database::accounts_groups",
                        "Skipping row with invalid dm_peer_pubkey '{}': {}",
                        peer_hex, e
                    );
                }
            }
        }
        Ok(results)
    }

    /// Finds visible DM groups for an account that are missing `dm_peer_pubkey`.
    ///
    /// Used by the startup backfill to identify records that need population.
    /// Pushes all filtering to SQL (joins with `group_information`) so the caller
    /// only receives rows that actually need MDK membership resolution.
    pub(crate) async fn find_dm_groups_missing_peer(
        account_pubkey: &PublicKey,
        database: &Database,
    ) -> Result<Vec<GroupId>, sqlx::Error> {
        let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
            "SELECT ag.mls_group_id
             FROM accounts_groups ag
             INNER JOIN group_information gi ON ag.mls_group_id = gi.mls_group_id
             WHERE ag.account_pubkey = ?
               AND ag.dm_peer_pubkey IS NULL
               AND gi.group_type = 'direct_message'
               AND (ag.user_confirmation IS NULL OR ag.user_confirmation = 1)",
        )
        .bind(account_pubkey.to_hex())
        .fetch_all(&database.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(bytes,)| GroupId::from_slice(&bytes))
            .collect())
    }

    /// Updates the dm_peer_pubkey for a specific account-group record.
    ///
    /// Used by the startup backfill to populate the column for existing DM groups.
    pub(crate) async fn update_dm_peer_pubkey(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        dm_peer_pubkey: &PublicKey,
        database: &Database,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE accounts_groups
             SET dm_peer_pubkey = ?, updated_at = ?
             WHERE account_pubkey = ? AND mls_group_id = ? AND dm_peer_pubkey IS NULL",
        )
        .bind(dm_peer_pubkey.to_hex())
        .bind(Utc::now().timestamp_millis())
        .bind(account_pubkey.to_hex())
        .bind(mls_group_id.as_slice())
        .execute(&database.pool)
        .await?;

        Ok(())
    }

    /// Creates a new AccountGroup with user_confirmation = NULL (pending).
    async fn create(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        welcomer_pubkey: Option<&PublicKey>,
        dm_peer_pubkey: Option<&PublicKey>,
        database: &Database,
    ) -> Result<Self, sqlx::Error> {
        let now_ms = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, AccountGroupRow>(
            "INSERT INTO accounts_groups (account_pubkey, mls_group_id, user_confirmation, welcomer_pubkey, dm_peer_pubkey, created_at, updated_at)
             VALUES (?, ?, NULL, ?, ?, ?, ?)
             RETURNING *",
        )
        .bind(account_pubkey.to_hex())
        .bind(mls_group_id.as_slice())
        .bind(welcomer_pubkey.map(|pk| pk.to_hex()))
        .bind(dm_peer_pubkey.map(|pk| pk.to_hex()))
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
    use crate::whitenoise::group_information::{GroupInformation, GroupType};
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
            AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
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
            AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
                .await
                .unwrap();
        assert!(was_created);

        // Second call should find existing
        let (found, was_created) =
            AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
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
            AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
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
            AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
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
            AccountGroup::find_or_create(&account.pubkey, &group_id1, None, &whitenoise.database)
                .await
                .unwrap();
        let (ag2, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id2, None, &whitenoise.database)
                .await
                .unwrap();
        let (ag3, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id3, None, &whitenoise.database)
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
            AccountGroup::find_or_create(&account.pubkey, &group_id1, None, &whitenoise.database)
                .await
                .unwrap();
        let (ag2, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id2, None, &whitenoise.database)
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
    async fn test_different_accounts_same_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account1 = whitenoise.create_identity().await.unwrap();
        let account2 = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[14; 32]);

        let (ag1, created1) =
            AccountGroup::find_or_create(&account1.pubkey, &group_id, None, &whitenoise.database)
                .await
                .unwrap();
        let (ag2, created2) =
            AccountGroup::find_or_create(&account2.pubkey, &group_id, None, &whitenoise.database)
                .await
                .unwrap();

        assert!(created1);
        assert!(created2);
        assert_ne!(ag1.id, ag2.id);
        assert_eq!(ag1.mls_group_id, ag2.mls_group_id);
        assert_ne!(ag1.account_pubkey, ag2.account_pubkey);
    }

    #[tokio::test]
    async fn test_find_by_group_empty() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[15; 32]);

        let result = AccountGroup::find_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_find_by_group_single_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[16; 32]);

        let (created_ag, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
                .await
                .unwrap();

        let result = AccountGroup::find_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, created_ag.id);
        assert_eq!(result[0].account_pubkey, account.pubkey);
        assert_eq!(result[0].mls_group_id, group_id);
    }

    #[tokio::test]
    async fn test_find_by_group_multiple_accounts() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account1 = whitenoise.create_identity().await.unwrap();
        let account2 = whitenoise.create_identity().await.unwrap();
        let account3 = whitenoise.create_identity().await.unwrap();
        let target_group = GroupId::from_slice(&[17; 32]);
        let other_group = GroupId::from_slice(&[18; 32]);

        // Add accounts 1 and 2 to the target group
        AccountGroup::find_or_create(&account1.pubkey, &target_group, None, &whitenoise.database)
            .await
            .unwrap();
        AccountGroup::find_or_create(&account2.pubkey, &target_group, None, &whitenoise.database)
            .await
            .unwrap();

        // Add account 3 to a different group (should not be returned)
        AccountGroup::find_or_create(&account3.pubkey, &other_group, None, &whitenoise.database)
            .await
            .unwrap();

        let result = AccountGroup::find_by_group(&target_group, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(result.len(), 2);

        let pubkeys: Vec<_> = result.iter().map(|ag| ag.account_pubkey).collect();
        assert!(pubkeys.contains(&account1.pubkey));
        assert!(pubkeys.contains(&account2.pubkey));
        assert!(!pubkeys.contains(&account3.pubkey)); // Should NOT be included

        // All returned should have the target group_id
        for ag in &result {
            assert_eq!(ag.mls_group_id, target_group);
        }
    }

    #[tokio::test]
    async fn test_save_creates_new_record() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let welcomer = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[30; 32]);

        let ag = AccountGroup {
            id: None,
            account_pubkey: account.pubkey,
            mls_group_id: group_id.clone(),
            user_confirmation: None,
            welcomer_pubkey: Some(welcomer.pubkey),
            last_read_message_id: None,
            pin_order: None,
            dm_peer_pubkey: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let saved = ag.save(&whitenoise.database).await.unwrap();

        assert!(saved.id.is_some());
        assert_eq!(saved.account_pubkey, account.pubkey);
        assert_eq!(saved.mls_group_id, group_id);
        assert!(saved.user_confirmation.is_none());
        assert_eq!(saved.welcomer_pubkey, Some(welcomer.pubkey));
        assert!(saved.pin_order.is_none());
    }

    #[tokio::test]
    async fn test_save_updates_existing_record() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let welcomer = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[31; 32]);

        // Create initial record with welcomer
        let ag = AccountGroup {
            id: None,
            account_pubkey: account.pubkey,
            mls_group_id: group_id.clone(),
            user_confirmation: Some(true),
            welcomer_pubkey: Some(welcomer.pubkey),
            last_read_message_id: None,
            pin_order: Some(100),
            dm_peer_pubkey: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let original = ag.save(&whitenoise.database).await.unwrap();
        assert_eq!(original.welcomer_pubkey, Some(welcomer.pubkey));
        assert_eq!(original.user_confirmation, Some(true));
        assert_eq!(original.pin_order, Some(100));

        // Save with None values - should overwrite existing values
        let update = AccountGroup {
            id: None,
            account_pubkey: account.pubkey,
            mls_group_id: group_id.clone(),
            user_confirmation: None,
            welcomer_pubkey: None,
            last_read_message_id: None,
            pin_order: None,
            dm_peer_pubkey: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let saved = update.save(&whitenoise.database).await.unwrap();

        assert_eq!(saved.id, original.id);
        assert!(saved.user_confirmation.is_none());
        assert!(saved.welcomer_pubkey.is_none());
        assert!(saved.pin_order.is_none());
    }

    #[tokio::test]
    async fn test_update_last_read_if_newer_sets_when_null() {
        use crate::whitenoise::aggregated_message::AggregatedMessage;
        use crate::whitenoise::group_information::{GroupInformation, GroupType};

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[40; 32]);

        // Setup group_information (FK constraint)
        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let message_id = EventId::all_zeros();
        let message_time = Utc::now();

        // Create the message
        AggregatedMessage::create_for_test(
            message_id,
            group_id.clone(),
            account.pubkey,
            message_time,
            &whitenoise.database,
        )
        .await
        .unwrap();

        let (account_group, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
                .await
                .unwrap();

        assert!(account_group.last_read_message_id.is_none());

        // Should update when last_read_message_id is NULL
        let updated = account_group
            .update_last_read_if_newer(
                &message_id,
                message_time.timestamp_millis(),
                &whitenoise.database,
            )
            .await
            .unwrap();

        assert!(updated.is_some());
        assert_eq!(updated.unwrap().last_read_message_id, Some(message_id));
    }

    #[tokio::test]
    async fn test_update_last_read_if_newer_advances_forward() {
        use crate::whitenoise::aggregated_message::AggregatedMessage;
        use crate::whitenoise::group_information::{GroupInformation, GroupType};

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[41; 32]);

        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let now = Utc::now();
        let older_time = now - chrono::Duration::seconds(10);
        let newer_time = now;

        let older_msg_id = EventId::from_hex(&format!("{:0>64}", "1")).unwrap();
        let newer_msg_id = EventId::from_hex(&format!("{:0>64}", "2")).unwrap();

        AggregatedMessage::create_for_test(
            older_msg_id,
            group_id.clone(),
            account.pubkey,
            older_time,
            &whitenoise.database,
        )
        .await
        .unwrap();

        AggregatedMessage::create_for_test(
            newer_msg_id,
            group_id.clone(),
            account.pubkey,
            newer_time,
            &whitenoise.database,
        )
        .await
        .unwrap();

        let (account_group, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
                .await
                .unwrap();

        // Set to older message first
        let updated = account_group
            .update_last_read_if_newer(
                &older_msg_id,
                older_time.timestamp_millis(),
                &whitenoise.database,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.last_read_message_id, Some(older_msg_id));

        // Should advance to newer message
        let updated = updated
            .update_last_read_if_newer(
                &newer_msg_id,
                newer_time.timestamp_millis(),
                &whitenoise.database,
            )
            .await
            .unwrap();

        assert!(updated.is_some());
        assert_eq!(updated.unwrap().last_read_message_id, Some(newer_msg_id));
    }

    #[tokio::test]
    async fn test_update_last_read_if_newer_rejects_older() {
        use crate::whitenoise::aggregated_message::AggregatedMessage;
        use crate::whitenoise::group_information::{GroupInformation, GroupType};

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[42; 32]);

        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let now = Utc::now();
        let older_time = now - chrono::Duration::seconds(10);
        let newer_time = now;

        let older_msg_id = EventId::from_hex(&format!("{:0>64}", "aaa")).unwrap();
        let newer_msg_id = EventId::from_hex(&format!("{:0>64}", "bbb")).unwrap();

        AggregatedMessage::create_for_test(
            older_msg_id,
            group_id.clone(),
            account.pubkey,
            older_time,
            &whitenoise.database,
        )
        .await
        .unwrap();

        AggregatedMessage::create_for_test(
            newer_msg_id,
            group_id.clone(),
            account.pubkey,
            newer_time,
            &whitenoise.database,
        )
        .await
        .unwrap();

        let (account_group, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
                .await
                .unwrap();

        // Set to newer message first
        let updated = account_group
            .update_last_read_if_newer(
                &newer_msg_id,
                newer_time.timestamp_millis(),
                &whitenoise.database,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.last_read_message_id, Some(newer_msg_id));

        // Should reject older message (returns None)
        let result = updated
            .update_last_read_if_newer(
                &older_msg_id,
                older_time.timestamp_millis(),
                &whitenoise.database,
            )
            .await
            .unwrap();

        assert!(result.is_none());

        // Verify the marker didn't change
        let found = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(found.last_read_message_id, Some(newer_msg_id));
    }

    #[tokio::test]
    async fn test_last_read_persists_through_find() {
        use crate::whitenoise::aggregated_message::AggregatedMessage;
        use crate::whitenoise::group_information::{GroupInformation, GroupType};

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[43; 32]);

        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let message_id = EventId::from_hex(&format!("{:0>64}", "abc")).unwrap();
        let message_time = Utc::now();

        AggregatedMessage::create_for_test(
            message_id,
            group_id.clone(),
            account.pubkey,
            message_time,
            &whitenoise.database,
        )
        .await
        .unwrap();

        let (account_group, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
                .await
                .unwrap();

        account_group
            .update_last_read_if_newer(
                &message_id,
                message_time.timestamp_millis(),
                &whitenoise.database,
            )
            .await
            .unwrap();

        // Fetch again and verify persistence
        let found = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(found.last_read_message_id, Some(message_id));
    }

    #[tokio::test]
    async fn test_update_pin_order_sets_value() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[60; 32]);

        let (account_group, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
                .await
                .unwrap();

        assert!(account_group.pin_order.is_none());

        let updated = account_group
            .update_pin_order(Some(42), &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(updated.pin_order, Some(42));
    }

    #[tokio::test]
    async fn test_update_pin_order_clears_value() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[61; 32]);

        let (account_group, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
                .await
                .unwrap();

        // Set pin order first
        let pinned = account_group
            .update_pin_order(Some(100), &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(pinned.pin_order, Some(100));

        // Clear pin order
        let unpinned = pinned
            .update_pin_order(None, &whitenoise.database)
            .await
            .unwrap();

        assert!(unpinned.pin_order.is_none());
    }

    #[tokio::test]
    async fn test_update_pin_order_persists() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[62; 32]);

        let (account_group, _) =
            AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
                .await
                .unwrap();

        account_group
            .update_pin_order(Some(77), &whitenoise.database)
            .await
            .unwrap();

        // Fetch again and verify persistence
        let found = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(found.pin_order, Some(77));
    }

    #[tokio::test]
    async fn test_find_dm_groups_missing_peer_returns_dm_without_peer() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[70; 32]);

        // Create group_information with DirectMessage type
        GroupInformation::create_for_group(
            &whitenoise,
            &group_id,
            Some(GroupType::DirectMessage),
            "",
        )
        .await
        .unwrap();

        // Create account_group WITHOUT dm_peer_pubkey (simulates pre-migration state)
        AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
            .await
            .unwrap();

        let missing =
            AccountGroup::find_dm_groups_missing_peer(&account.pubkey, &whitenoise.database)
                .await
                .unwrap();

        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0], group_id);
    }

    #[tokio::test]
    async fn test_find_dm_groups_missing_peer_excludes_groups_with_peer() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let peer = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[71; 32]);

        // Create group_information with DirectMessage type
        GroupInformation::create_for_group(
            &whitenoise,
            &group_id,
            Some(GroupType::DirectMessage),
            "",
        )
        .await
        .unwrap();

        // Create account_group WITH dm_peer_pubkey (already populated)
        AccountGroup::find_or_create(
            &account.pubkey,
            &group_id,
            Some(&peer.pubkey),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let missing =
            AccountGroup::find_dm_groups_missing_peer(&account.pubkey, &whitenoise.database)
                .await
                .unwrap();

        assert!(missing.is_empty());
    }

    #[tokio::test]
    async fn test_find_dm_groups_missing_peer_excludes_regular_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[72; 32]);

        // Create group_information with Group type (not DM)
        GroupInformation::create_for_group(
            &whitenoise,
            &group_id,
            Some(GroupType::Group),
            "Team Chat",
        )
        .await
        .unwrap();

        // Create account_group without dm_peer_pubkey
        AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
            .await
            .unwrap();

        let missing =
            AccountGroup::find_dm_groups_missing_peer(&account.pubkey, &whitenoise.database)
                .await
                .unwrap();

        // Should not include regular groups even if dm_peer_pubkey is NULL
        assert!(missing.is_empty());
    }

    #[tokio::test]
    async fn test_update_dm_peer_pubkey_sets_peer() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let peer = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[73; 32]);

        // Create account_group without dm_peer_pubkey
        AccountGroup::find_or_create(&account.pubkey, &group_id, None, &whitenoise.database)
            .await
            .unwrap();

        // Update the dm_peer_pubkey
        AccountGroup::update_dm_peer_pubkey(
            &account.pubkey,
            &group_id,
            &peer.pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Verify it was set
        let found = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(found.dm_peer_pubkey, Some(peer.pubkey));
    }

    #[tokio::test]
    async fn test_update_dm_peer_pubkey_does_not_overwrite_existing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let original_peer = whitenoise.create_identity().await.unwrap();
        let new_peer = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[74; 32]);

        // Create account_group WITH dm_peer_pubkey already set
        AccountGroup::find_or_create(
            &account.pubkey,
            &group_id,
            Some(&original_peer.pubkey),
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Try to update with a different peer
        AccountGroup::update_dm_peer_pubkey(
            &account.pubkey,
            &group_id,
            &new_peer.pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Verify original peer is preserved (WHERE clause includes dm_peer_pubkey IS NULL)
        let found = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(found.dm_peer_pubkey, Some(original_peer.pubkey));
    }
}

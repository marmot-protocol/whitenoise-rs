use chrono::{DateTime, Utc};
use mdk_core::prelude::GroupId;
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;

use crate::perf_instrument;
use crate::whitenoise::accounts_groups::AccountGroup;

/// Per-account row shape. The per-account DB does not store `account_pubkey`
/// (the file *is* the scope), so this intermediate struct decodes what the DB
/// actually contains and converts to the domain `AccountGroup` via
/// `into_account_group`.
#[derive(sqlx::FromRow)]
struct LocalAccountGroupRow {
    id: i64,
    mls_group_id: Vec<u8>,
    user_confirmation: Option<i64>,
    welcomer_pubkey: Option<String>,
    last_read_message_id: Option<String>,
    pin_order: Option<i64>,
    dm_peer_pubkey: Option<String>,
    archived_at: Option<i64>,
    removed_at: Option<i64>,
    self_removed: i64,
    muted_until: Option<i64>,
    chat_cleared_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

impl LocalAccountGroupRow {
    fn into_account_group(self, account_pubkey: PublicKey) -> Result<AccountGroup, sqlx::Error> {
        let welcomer_pubkey = match self.welcomer_pubkey {
            Some(s) => Some(PublicKey::parse(&s).map_err(|e| sqlx::Error::ColumnDecode {
                index: "welcomer_pubkey".to_string(),
                source: Box::new(e),
            })?),
            None => None,
        };

        let last_read_message_id = match self.last_read_message_id {
            Some(hex) => Some(
                EventId::from_hex(&hex).map_err(|e| sqlx::Error::ColumnDecode {
                    index: "last_read_message_id".to_string(),
                    source: Box::new(e),
                })?,
            ),
            None => None,
        };

        let dm_peer_pubkey = match self.dm_peer_pubkey {
            Some(s) => Some(PublicKey::parse(&s).map_err(|e| sqlx::Error::ColumnDecode {
                index: "dm_peer_pubkey".to_string(),
                source: Box::new(e),
            })?),
            None => None,
        };

        let user_confirmation = match self.user_confirmation {
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

        let mls_group_id = GroupId::from_slice(&self.mls_group_id);

        let archived_at = parse_optional_timestamp_from_millis(self.archived_at)?;
        let removed_at = parse_optional_timestamp_from_millis(self.removed_at)?;
        let muted_until = parse_optional_timestamp_from_millis(self.muted_until)?;
        let chat_cleared_at = parse_optional_timestamp_from_millis(self.chat_cleared_at)?;
        let created_at = parse_timestamp_from_millis(self.created_at)?;
        let updated_at = parse_timestamp_from_millis(self.updated_at)?;

        Ok(AccountGroup {
            id: Some(self.id),
            account_pubkey,
            mls_group_id,
            user_confirmation,
            welcomer_pubkey,
            last_read_message_id,
            pin_order: self.pin_order,
            dm_peer_pubkey,
            archived_at,
            removed_at,
            self_removed: self.self_removed != 0,
            muted_until,
            chat_cleared_at,
            created_at,
            updated_at,
        })
    }
}

fn parse_timestamp_from_millis(ms: i64) -> Result<DateTime<Utc>, sqlx::Error> {
    DateTime::from_timestamp_millis(ms).ok_or_else(|| sqlx::Error::ColumnDecode {
        index: "timestamp".to_string(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Invalid timestamp millis: {}", ms),
        )),
    })
}

fn parse_optional_timestamp_from_millis(
    ms: Option<i64>,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    match ms {
        None => Ok(None),
        Some(v) => Ok(Some(parse_timestamp_from_millis(v)?)),
    }
}

impl AccountGroup {
    /// Finds an AccountGroup by MLS group ID in the per-account database.
    ///
    /// `account_pubkey` is used to populate the returned struct but is NOT
    /// used as a SQL filter (the per-account DB is implicitly scoped).
    #[perf_instrument("db::accounts_groups")]
    pub async fn find_by_account_and_group(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        pool: &SqlitePool,
    ) -> Result<Option<Self>, sqlx::Error> {
        let row = sqlx::query_as::<_, LocalAccountGroupRow>(
            "SELECT id, mls_group_id, user_confirmation, welcomer_pubkey,
                    last_read_message_id, pin_order, dm_peer_pubkey,
                    archived_at, removed_at, self_removed, muted_until,
                    chat_cleared_at, created_at, updated_at
             FROM accounts_groups
             WHERE mls_group_id = ?",
        )
        .bind(mls_group_id.as_slice())
        .fetch_optional(pool)
        .await?;

        match row {
            Some(r) => Ok(Some(r.into_account_group(*account_pubkey)?)),
            None => Ok(None),
        }
    }

    /// Finds or creates an AccountGroup for the given group.
    /// Returns the AccountGroup and a boolean indicating if it was newly created.
    ///
    /// This uses an insert-first approach to avoid TOCTOU race conditions:
    /// - Attempts to insert first
    /// - On unique constraint violation, fetches the existing record
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn find_or_create(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        dm_peer_pubkey: Option<&PublicKey>,
        pool: &SqlitePool,
    ) -> Result<(Self, bool), sqlx::Error> {
        // Try to create first - this handles the race condition properly
        match Self::create(account_pubkey, mls_group_id, None, dm_peer_pubkey, pool).await {
            Ok(created) => Ok((created, true)),
            Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
                // Insert failed due to unique constraint - a concurrent task already created the row
                // Fall back to fetching the existing record
                let existing = Self::find_by_account_and_group(account_pubkey, mls_group_id, pool)
                    .await?
                    .ok_or(sqlx::Error::RowNotFound)?;
                Ok((existing, false))
            }
            Err(e) => Err(e),
        }
    }

    /// Finds all visible AccountGroups for the account.
    /// Visible means: user_confirmation is NULL (pending) or true (accepted).
    /// Declined groups (user_confirmation = false) are hidden.
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn find_visible_for_account(
        account_pubkey: &PublicKey,
        pool: &SqlitePool,
    ) -> Result<Vec<Self>, sqlx::Error> {
        let rows = sqlx::query_as::<_, LocalAccountGroupRow>(
            "SELECT id, mls_group_id, user_confirmation, welcomer_pubkey,
                    last_read_message_id, pin_order, dm_peer_pubkey,
                    archived_at, removed_at, self_removed, muted_until,
                    chat_cleared_at, created_at, updated_at
             FROM accounts_groups
             WHERE user_confirmation IS NULL OR user_confirmation = 1",
        )
        .fetch_all(pool)
        .await?;

        rows.into_iter()
            .map(|r| r.into_account_group(*account_pubkey))
            .collect()
    }

    /// Finds all pending AccountGroups for the account.
    /// Pending means: user_confirmation is NULL.
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn find_pending_for_account(
        account_pubkey: &PublicKey,
        pool: &SqlitePool,
    ) -> Result<Vec<Self>, sqlx::Error> {
        let rows = sqlx::query_as::<_, LocalAccountGroupRow>(
            "SELECT id, mls_group_id, user_confirmation, welcomer_pubkey,
                    last_read_message_id, pin_order, dm_peer_pubkey,
                    archived_at, removed_at, self_removed, muted_until,
                    chat_cleared_at, created_at, updated_at
             FROM accounts_groups
             WHERE user_confirmation IS NULL
               AND removed_at IS NULL",
        )
        .fetch_all(pool)
        .await?;

        rows.into_iter()
            .map(|r| r.into_account_group(*account_pubkey))
            .collect()
    }

    /// Updates the user_confirmation status for this AccountGroup.
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn update_user_confirmation(
        &self,
        user_confirmation: bool,
        pool: &SqlitePool,
    ) -> Result<Self, sqlx::Error> {
        let id = self.id.expect("AccountGroup must be persisted");

        let now_ms = Utc::now().timestamp_millis();
        let confirmation_int: i64 = if user_confirmation { 1 } else { 0 };

        let row = sqlx::query_as::<_, LocalAccountGroupRow>(
            "UPDATE accounts_groups
             SET user_confirmation = ?, updated_at = ?
             WHERE id = ?
             RETURNING *",
        )
        .bind(confirmation_int)
        .bind(now_ms)
        .bind(id)
        .fetch_one(pool)
        .await?;

        row.into_account_group(self.account_pubkey)
    }

    /// Saves the AccountGroup to the database (upsert).
    ///
    /// - If the record doesn't exist, inserts it
    /// - If it exists, updates all mutable fields to match the provided values
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn save(&self, pool: &SqlitePool) -> Result<Self, sqlx::Error> {
        let now_ms = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, LocalAccountGroupRow>(
            "INSERT INTO accounts_groups (mls_group_id, user_confirmation, welcomer_pubkey, last_read_message_id, pin_order, dm_peer_pubkey, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(mls_group_id) DO UPDATE SET
               user_confirmation = excluded.user_confirmation,
               welcomer_pubkey = excluded.welcomer_pubkey,
               last_read_message_id = excluded.last_read_message_id,
               pin_order = excluded.pin_order,
               -- archived_at is intentionally excluded: archive/unarchive use
               -- update_archived_at() so save() never clobbers user preference.
               -- removed_at and self_removed are intentionally excluded:
               -- set by mark_removed_atomic() or mark_left_atomic() only.
               -- muted_until is intentionally excluded: mute/unmute use
               -- update_muted_until() so save() never clobbers user preference.
               -- Write-once: preserve existing dm_peer_pubkey if already set.
               -- Many code paths construct AccountGroup without knowing the DM peer,
               -- so we only fill this on first write and never overwrite a correct
               -- value with NULL.
               dm_peer_pubkey = COALESCE(accounts_groups.dm_peer_pubkey, excluded.dm_peer_pubkey),
               updated_at = excluded.updated_at
             RETURNING *",
        )
        .bind(self.mls_group_id.as_slice())
        .bind(self.user_confirmation.map(|b| if b { 1i64 } else { 0i64 }))
        .bind(self.welcomer_pubkey.as_ref().map(|pk| pk.to_hex()))
        .bind(self.last_read_message_id.as_ref().map(|id| id.to_hex()))
        .bind(self.pin_order)
        .bind(self.dm_peer_pubkey.as_ref().map(|pk| pk.to_hex()))
        .bind(self.created_at.timestamp_millis())
        .bind(now_ms)
        .fetch_one(pool)
        .await?;

        row.into_account_group(self.account_pubkey)
    }

    /// Updates the pin_order for this AccountGroup.
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn update_pin_order(
        &self,
        pin_order: Option<i64>,
        pool: &SqlitePool,
    ) -> Result<Self, sqlx::Error> {
        let id = self.id.expect("AccountGroup must be persisted");
        let now_ms = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, LocalAccountGroupRow>(
            "UPDATE accounts_groups
             SET pin_order = ?, updated_at = ?
             WHERE id = ?
             RETURNING *",
        )
        .bind(pin_order)
        .bind(now_ms)
        .bind(id)
        .fetch_one(pool)
        .await?;

        row.into_account_group(self.account_pubkey)
    }

    /// Updates the archived_at timestamp for this AccountGroup.
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn update_archived_at(
        &self,
        archived_at: Option<DateTime<Utc>>,
        pool: &SqlitePool,
    ) -> Result<Self, sqlx::Error> {
        let id = self.id.expect("AccountGroup must be persisted");
        let now_ms = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, LocalAccountGroupRow>(
            "UPDATE accounts_groups
             SET archived_at = ?, updated_at = ?
             WHERE id = ?
             RETURNING *",
        )
        .bind(archived_at.map(|dt| dt.timestamp_millis()))
        .bind(now_ms)
        .bind(id)
        .fetch_one(pool)
        .await?;

        row.into_account_group(self.account_pubkey)
    }

    /// Updates the muted_until timestamp for this AccountGroup.
    ///
    /// - `Some(until)` = muted until that time
    /// - `None` = unmuted
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn update_muted_until(
        &self,
        muted_until: Option<DateTime<Utc>>,
        pool: &SqlitePool,
    ) -> Result<Self, sqlx::Error> {
        let id = self.id.expect("AccountGroup must be persisted");
        let now_ms = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, LocalAccountGroupRow>(
            "UPDATE accounts_groups
             SET muted_until = ?, updated_at = ?
             WHERE id = ?
             RETURNING *",
        )
        .bind(muted_until.map(|dt| dt.timestamp_millis()))
        .bind(now_ms)
        .bind(id)
        .fetch_one(pool)
        .await?;

        row.into_account_group(self.account_pubkey)
    }

    /// Updates the chat_cleared_at timestamp for this AccountGroup.
    ///
    /// - `Some(timestamp)` = messages at or before this time are hidden
    /// - `None` = not cleared (all messages visible)
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn update_chat_cleared_at(
        &self,
        chat_cleared_at: Option<DateTime<Utc>>,
        pool: &SqlitePool,
    ) -> Result<Self, sqlx::Error> {
        let id = self.id.expect("AccountGroup must be persisted");
        let now_ms = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, LocalAccountGroupRow>(
            "UPDATE accounts_groups
             SET chat_cleared_at = ?, updated_at = ?
             WHERE id = ?
             RETURNING *",
        )
        .bind(chat_cleared_at.map(|dt| dt.timestamp_millis()))
        .bind(now_ms)
        .bind(id)
        .fetch_one(pool)
        .await?;

        row.into_account_group(self.account_pubkey)
    }

    /// Clears the last_read_message_id for this AccountGroup.
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn reset_last_read_message_id(
        &self,
        pool: &SqlitePool,
    ) -> Result<Self, sqlx::Error> {
        let id = self.id.expect("AccountGroup must be persisted");
        let now_ms = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, LocalAccountGroupRow>(
            "UPDATE accounts_groups
             SET last_read_message_id = NULL, updated_at = ?
             WHERE id = ?
             RETURNING *",
        )
        .bind(now_ms)
        .bind(id)
        .fetch_one(pool)
        .await?;

        row.into_account_group(self.account_pubkey)
    }

    /// Sets `chat_cleared_at` and resets `last_read_message_id` in a single atomic write.
    ///
    /// Used by `clear_chat` to ensure both fields are updated together, preventing
    /// a race with `mark_message_read` that could leave a stale read marker.
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn clear_chat_state(
        &self,
        chat_cleared_at: DateTime<Utc>,
        pool: &SqlitePool,
    ) -> Result<Self, sqlx::Error> {
        let id = self.id.expect("AccountGroup must be persisted");
        let now_ms = Utc::now().timestamp_millis();
        let cleared_ms = chat_cleared_at.timestamp_millis();

        let row = sqlx::query_as::<_, LocalAccountGroupRow>(
            "UPDATE accounts_groups
             SET chat_cleared_at = ?, last_read_message_id = NULL, updated_at = ?
             WHERE id = ?
             RETURNING *",
        )
        .bind(cleared_ms)
        .bind(now_ms)
        .bind(id)
        .fetch_one(pool)
        .await?;

        row.into_account_group(self.account_pubkey)
    }

    /// Clears `muted_until` for all rows whose mute has expired in this
    /// per-account database.
    ///
    /// Returns the affected rows so callers can emit stream updates.
    /// Callers that need cross-account behaviour must iterate all sessions.
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn clear_expired_mutes(
        account_pubkey: &PublicKey,
        pool: &SqlitePool,
    ) -> Result<Vec<Self>, sqlx::Error> {
        let now_ms = Utc::now().timestamp_millis();

        let rows = sqlx::query_as::<_, LocalAccountGroupRow>(
            "UPDATE accounts_groups
             SET muted_until = NULL, updated_at = ?
             WHERE muted_until IS NOT NULL AND muted_until <= ?
             RETURNING *",
        )
        .bind(now_ms)
        .bind(now_ms)
        .fetch_all(pool)
        .await?;

        rows.into_iter()
            .map(|r| r.into_account_group(*account_pubkey))
            .collect()
    }

    /// Atomically marks this AccountGroup as voluntarily departed
    /// (`UPDATE ... WHERE removed_at IS NULL`).
    ///
    /// Sets both `removed_at` and `self_removed` in one statement.
    /// Returns `Some(updated_row)` on success, `None` if already departed
    /// (another task won the race, or the user was already removed/left).
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn mark_left_atomic(
        &self,
        pool: &SqlitePool,
    ) -> Result<Option<Self>, sqlx::Error> {
        let id = self.id.expect("AccountGroup must be persisted");
        let now_ms = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, LocalAccountGroupRow>(
            "UPDATE accounts_groups
             SET removed_at = ?, self_removed = 1, updated_at = ?
             WHERE id = ? AND removed_at IS NULL
             RETURNING *",
        )
        .bind(now_ms)
        .bind(now_ms)
        .bind(id)
        .fetch_optional(pool)
        .await?;

        match row {
            Some(r) => Ok(Some(r.into_account_group(self.account_pubkey)?)),
            None => Ok(None),
        }
    }

    /// Atomically marks this AccountGroup as removed (`UPDATE … WHERE removed_at IS NULL`).
    ///
    /// Returns the updated row if changed, `None` if already removed.
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn mark_removed_atomic(
        &self,
        pool: &SqlitePool,
    ) -> Result<Option<Self>, sqlx::Error> {
        let id = self.id.expect("AccountGroup must be persisted");
        let now_ms = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, LocalAccountGroupRow>(
            "UPDATE accounts_groups
             SET removed_at = ?, updated_at = ?
             WHERE id = ? AND removed_at IS NULL
             RETURNING *",
        )
        .bind(now_ms)
        .bind(now_ms)
        .bind(id)
        .fetch_optional(pool)
        .await?;

        match row {
            Some(r) => Ok(Some(r.into_account_group(self.account_pubkey)?)),
            None => Ok(None),
        }
    }

    /// Atomically updates last_read_message_id only if the new message is
    /// newer than the current marker.
    ///
    /// Uses optimistic concurrency (CAS): the UPDATE includes a
    /// `last_read_message_id IS ?expected` guard so concurrent writers cannot
    /// regress the marker. Returns `Some(updated)` on success, `None` if the
    /// new message is not newer than the current marker.
    ///
    /// `expected_marker_id` must be the current `last_read_message_id` value
    /// (or `None` if there is no marker). If another writer changed it between
    /// the caller's read and this write, 0 rows are updated — the caller
    /// should re-read and retry.
    ///
    /// `current_marker_created_at_ms` is the `created_at` timestamp of the
    /// current marker (from `aggregated_messages`). Pass `0` if there is no
    /// current marker.
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn update_last_read_if_newer(
        &self,
        message_id: &EventId,
        message_created_at_ms: i64,
        expected_marker_id: Option<&EventId>,
        current_marker_created_at_ms: i64,
        pool: &SqlitePool,
    ) -> Result<Option<Self>, sqlx::Error> {
        let id = self.id.expect("AccountGroup must be persisted");
        let now_ms = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, LocalAccountGroupRow>(
            "UPDATE accounts_groups
             SET last_read_message_id = ?, updated_at = ?
             WHERE id = ?
               AND last_read_message_id IS ?
               AND (
                 last_read_message_id IS NULL
                 OR ? > ?
               )
             RETURNING *",
        )
        .bind(message_id.to_hex())
        .bind(now_ms)
        .bind(id)
        .bind(expected_marker_id.map(|eid| eid.to_hex()))
        .bind(message_created_at_ms)
        .bind(current_marker_created_at_ms)
        .fetch_optional(pool)
        .await?;

        match row {
            Some(r) => Ok(Some(r.into_account_group(self.account_pubkey)?)),
            None => Ok(None),
        }
    }

    /// Finds the most recently created visible DM group with a given peer.
    ///
    /// Uses the `dm_peer_pubkey` column for an efficient single-query lookup
    /// without requiring MLS/MDK calls. Returns `None` if no DM group exists
    /// with this peer, or if the group has been declined.
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn find_dm_group_id_by_peer(
        peer_pubkey: &PublicKey,
        pool: &SqlitePool,
    ) -> Result<Option<GroupId>, sqlx::Error> {
        let row: Option<(Vec<u8>,)> = sqlx::query_as(
            "SELECT mls_group_id
             FROM accounts_groups
             WHERE dm_peer_pubkey = ?
               AND (user_confirmation IS NULL OR user_confirmation = 1)
             ORDER BY created_at DESC
             LIMIT 1",
        )
        .bind(peer_pubkey.to_hex())
        .fetch_optional(pool)
        .await?;

        Ok(row.map(|(bytes,)| GroupId::from_slice(&bytes)))
    }

    /// Returns DM peer pubkeys for all visible DM groups in this account.
    ///
    /// Returns `(mls_group_id, dm_peer_pubkey)` pairs for groups where
    /// `dm_peer_pubkey` is populated and the group is visible (not declined).
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn find_dm_peers_for_account(
        pool: &SqlitePool,
    ) -> Result<Vec<(GroupId, PublicKey)>, sqlx::Error> {
        let rows: Vec<(Vec<u8>, String)> = sqlx::query_as(
            "SELECT mls_group_id, dm_peer_pubkey
             FROM accounts_groups
             WHERE dm_peer_pubkey IS NOT NULL
               AND (user_confirmation IS NULL OR user_confirmation = 1)",
        )
        .fetch_all(pool)
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

    /// Finds visible groups that are missing `dm_peer_pubkey`.
    ///
    /// Used by the startup backfill to identify records that need population.
    /// Returns all visible groups with `dm_peer_pubkey IS NULL`; the caller
    /// filters against `group_information` in the shared DB to find only DM
    /// groups (cross-DB join replaced by application-level join).
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn find_groups_missing_peer(
        pool: &SqlitePool,
    ) -> Result<Vec<GroupId>, sqlx::Error> {
        let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
            "SELECT mls_group_id
             FROM accounts_groups
             WHERE dm_peer_pubkey IS NULL
               AND (user_confirmation IS NULL OR user_confirmation = 1)",
        )
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(bytes,)| GroupId::from_slice(&bytes))
            .collect())
    }

    /// Updates the dm_peer_pubkey for a specific group record.
    ///
    /// Used by the startup backfill to populate the column for existing DM groups.
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn update_dm_peer_pubkey(
        mls_group_id: &GroupId,
        dm_peer_pubkey: &PublicKey,
        pool: &SqlitePool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE accounts_groups
             SET dm_peer_pubkey = ?, updated_at = ?
             WHERE mls_group_id = ? AND dm_peer_pubkey IS NULL",
        )
        .bind(dm_peer_pubkey.to_hex())
        .bind(Utc::now().timestamp_millis())
        .bind(mls_group_id.as_slice())
        .execute(pool)
        .await?;

        Ok(())
    }

    /// Creates a new AccountGroup with user_confirmation = NULL (pending).
    #[perf_instrument("db::accounts_groups")]
    async fn create(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        welcomer_pubkey: Option<&PublicKey>,
        dm_peer_pubkey: Option<&PublicKey>,
        pool: &SqlitePool,
    ) -> Result<Self, sqlx::Error> {
        let now_ms = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, LocalAccountGroupRow>(
            "INSERT INTO accounts_groups (mls_group_id, user_confirmation, welcomer_pubkey, dm_peer_pubkey, created_at, updated_at)
             VALUES (?, NULL, ?, ?, ?, ?)
             RETURNING *",
        )
        .bind(mls_group_id.as_slice())
        .bind(welcomer_pubkey.map(|pk| pk.to_hex()))
        .bind(dm_peer_pubkey.map(|pk| pk.to_hex()))
        .bind(now_ms)
        .bind(now_ms)
        .fetch_one(pool)
        .await?;

        row.into_account_group(*account_pubkey)
    }
    /// Deletes all accounts_groups rows for the given group in this per-account DB.
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn delete_for_group(
        mls_group_id: &GroupId,
        pool: &SqlitePool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM accounts_groups WHERE mls_group_id = ?")
            .bind(mls_group_id.as_slice())
            .execute(pool)
            .await?;
        Ok(())
    }

    /// Returns whether an accounts_groups row exists for the given group in this per-account DB.
    #[perf_instrument("db::accounts_groups")]
    pub(crate) async fn exists_for_group(
        mls_group_id: &GroupId,
        pool: &SqlitePool,
    ) -> Result<bool, sqlx::Error> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM accounts_groups WHERE mls_group_id = ?)",
        )
        .bind(mls_group_id.as_slice())
        .fetch_one(pool)
        .await?;
        Ok(exists)
    }
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;
    use tempfile::TempDir;

    use super::*;

    async fn test_pool() -> (SqlitePool, TempDir) {
        let dir = TempDir::new().unwrap();
        let pool = SqlitePool::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("test.db").display()
        ))
        .await
        .unwrap();

        sqlx::query(
            "CREATE TABLE accounts_groups (
                id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                mls_group_id         BLOB NOT NULL UNIQUE,
                user_confirmation    INTEGER DEFAULT NULL
                    CHECK (user_confirmation IS NULL OR user_confirmation IN (0, 1)),
                welcomer_pubkey      TEXT,
                last_read_message_id TEXT,
                pin_order            INTEGER DEFAULT NULL,
                dm_peer_pubkey       TEXT DEFAULT NULL,
                archived_at          INTEGER DEFAULT NULL,
                removed_at           INTEGER DEFAULT NULL,
                self_removed         INTEGER NOT NULL DEFAULT 0,
                muted_until          INTEGER DEFAULT NULL,
                chat_cleared_at      INTEGER DEFAULT NULL,
                created_at           INTEGER NOT NULL,
                updated_at           INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        (pool, dir)
    }

    fn test_pubkey() -> PublicKey {
        PublicKey::parse(&"ab".repeat(32)).unwrap()
    }

    #[tokio::test]
    async fn test_find_by_account_and_group_not_found() {
        let (pool, _dir) = test_pool().await;
        let pubkey = test_pubkey();
        let group_id = GroupId::from_slice(&[1; 32]);

        let result = AccountGroup::find_by_account_and_group(&pubkey, &group_id, &pool)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_find_or_create_creates_new() {
        let (pool, _dir) = test_pool().await;
        let pubkey = test_pubkey();
        let group_id = GroupId::from_slice(&[2; 32]);

        let (ag, was_created) = AccountGroup::find_or_create(&pubkey, &group_id, None, &pool)
            .await
            .unwrap();

        assert!(was_created);
        assert_eq!(ag.account_pubkey, pubkey);
        assert_eq!(ag.mls_group_id, group_id);
        assert!(ag.user_confirmation.is_none());
        assert!(ag.id.is_some());
    }

    #[tokio::test]
    async fn test_find_or_create_finds_existing() {
        let (pool, _dir) = test_pool().await;
        let pubkey = test_pubkey();
        let group_id = GroupId::from_slice(&[3; 32]);

        let (first, _) = AccountGroup::find_or_create(&pubkey, &group_id, None, &pool)
            .await
            .unwrap();

        let (second, was_created) = AccountGroup::find_or_create(&pubkey, &group_id, None, &pool)
            .await
            .unwrap();

        assert!(!was_created);
        assert_eq!(first.id, second.id);
    }

    #[tokio::test]
    async fn test_update_user_confirmation() {
        let (pool, _dir) = test_pool().await;
        let pubkey = test_pubkey();
        let group_id = GroupId::from_slice(&[4; 32]);

        let (ag, _) = AccountGroup::find_or_create(&pubkey, &group_id, None, &pool)
            .await
            .unwrap();
        assert!(ag.user_confirmation.is_none());

        let updated = ag.update_user_confirmation(true, &pool).await.unwrap();
        assert_eq!(updated.user_confirmation, Some(true));
    }

    #[tokio::test]
    async fn test_save_upsert() {
        let (pool, _dir) = test_pool().await;
        let pubkey = test_pubkey();
        let group_id = GroupId::from_slice(&[5; 32]);

        let ag = AccountGroup {
            id: None,
            account_pubkey: pubkey,
            mls_group_id: group_id,
            user_confirmation: Some(true),
            welcomer_pubkey: None,
            last_read_message_id: None,
            pin_order: Some(10),
            dm_peer_pubkey: None,
            archived_at: None,
            removed_at: None,
            self_removed: false,
            muted_until: None,
            chat_cleared_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let saved = ag.save(&pool).await.unwrap();
        assert!(saved.id.is_some());
        assert_eq!(saved.pin_order, Some(10));
        assert_eq!(saved.user_confirmation, Some(true));
    }

    #[tokio::test]
    async fn test_find_visible_and_pending() {
        let (pool, _dir) = test_pool().await;
        let pubkey = test_pubkey();

        // Create 3 groups: pending, accepted, declined
        let (pending, _) =
            AccountGroup::find_or_create(&pubkey, &GroupId::from_slice(&[10; 32]), None, &pool)
                .await
                .unwrap();

        let (accepted, _) =
            AccountGroup::find_or_create(&pubkey, &GroupId::from_slice(&[11; 32]), None, &pool)
                .await
                .unwrap();
        accepted
            .update_user_confirmation(true, &pool)
            .await
            .unwrap();

        let (declined, _) =
            AccountGroup::find_or_create(&pubkey, &GroupId::from_slice(&[12; 32]), None, &pool)
                .await
                .unwrap();
        declined
            .update_user_confirmation(false, &pool)
            .await
            .unwrap();

        let visible = AccountGroup::find_visible_for_account(&pubkey, &pool)
            .await
            .unwrap();
        assert_eq!(visible.len(), 2); // pending + accepted

        let pendings = AccountGroup::find_pending_for_account(&pubkey, &pool)
            .await
            .unwrap();
        assert_eq!(pendings.len(), 1);
        assert_eq!(pendings[0].mls_group_id, pending.mls_group_id);
    }

    #[tokio::test]
    async fn test_update_last_read_if_newer() {
        let (pool, _dir) = test_pool().await;
        let pubkey = test_pubkey();
        let group_id = GroupId::from_slice(&[20; 32]);

        let (ag, _) = AccountGroup::find_or_create(&pubkey, &group_id, None, &pool)
            .await
            .unwrap();

        let msg_id = EventId::from_hex(&"aa".repeat(32)).unwrap();

        // First read: no current marker (expected=None, ts=0)
        let updated = ag
            .update_last_read_if_newer(&msg_id, 1000, None, 0, &pool)
            .await
            .unwrap();
        assert!(updated.is_some());
        assert_eq!(updated.unwrap().last_read_message_id, Some(msg_id));

        // Re-read fresh row
        let ag = AccountGroup::find_by_account_and_group(&pubkey, &group_id, &pool)
            .await
            .unwrap()
            .unwrap();

        // Older message: should NOT update (CAS expected=msg_id matches)
        let older_id = EventId::from_hex(&"bb".repeat(32)).unwrap();
        let not_updated = ag
            .update_last_read_if_newer(&older_id, 500, Some(&msg_id), 1000, &pool)
            .await
            .unwrap();
        assert!(not_updated.is_none());

        // Newer message: should update (CAS expected=msg_id matches)
        let newer_id = EventId::from_hex(&"cc".repeat(32)).unwrap();
        let updated = ag
            .update_last_read_if_newer(&newer_id, 2000, Some(&msg_id), 1000, &pool)
            .await
            .unwrap();
        assert!(updated.is_some());
        assert_eq!(updated.unwrap().last_read_message_id, Some(newer_id));

        // CAS miss: expected=msg_id but current is now newer_id → 0 rows
        let newest_id = EventId::from_hex(&"dd".repeat(32)).unwrap();
        let cas_miss = ag
            .update_last_read_if_newer(&newest_id, 3000, Some(&msg_id), 1000, &pool)
            .await
            .unwrap();
        assert!(cas_miss.is_none(), "CAS must reject stale expected marker");
    }

    #[tokio::test]
    async fn test_dm_peer_pubkey_operations() {
        let (pool, _dir) = test_pool().await;
        let pubkey = test_pubkey();
        let peer = PublicKey::parse(&"cd".repeat(32)).unwrap();
        let group_id = GroupId::from_slice(&[30; 32]);

        // Create with dm_peer_pubkey
        let (ag, _) = AccountGroup::find_or_create(&pubkey, &group_id, Some(&peer), &pool)
            .await
            .unwrap();
        assert_eq!(ag.dm_peer_pubkey, Some(peer));

        // find_dm_group_id_by_peer
        let found = AccountGroup::find_dm_group_id_by_peer(&peer, &pool)
            .await
            .unwrap();
        assert_eq!(found, Some(group_id.clone()));

        // find_dm_peers_for_account
        let peers = AccountGroup::find_dm_peers_for_account(&pool)
            .await
            .unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].0, group_id);
        assert_eq!(peers[0].1, peer);
    }

    #[tokio::test]
    async fn test_find_groups_missing_peer() {
        let (pool, _dir) = test_pool().await;
        let pubkey = test_pubkey();

        // Group with peer
        AccountGroup::find_or_create(
            &pubkey,
            &GroupId::from_slice(&[40; 32]),
            Some(&PublicKey::parse(&"ee".repeat(32)).unwrap()),
            &pool,
        )
        .await
        .unwrap();

        // Group without peer
        AccountGroup::find_or_create(&pubkey, &GroupId::from_slice(&[41; 32]), None, &pool)
            .await
            .unwrap();

        let missing = AccountGroup::find_groups_missing_peer(&pool).await.unwrap();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0], GroupId::from_slice(&[41; 32]));
    }

    #[tokio::test]
    async fn test_clear_expired_mutes() {
        let (pool, _dir) = test_pool().await;
        let pubkey = test_pubkey();
        let group_id = GroupId::from_slice(&[50; 32]);

        let (ag, _) = AccountGroup::find_or_create(&pubkey, &group_id, None, &pool)
            .await
            .unwrap();

        // Set an already-expired mute
        let past = Utc::now() - chrono::Duration::hours(1);
        ag.update_muted_until(Some(past), &pool).await.unwrap();

        let cleared = AccountGroup::clear_expired_mutes(&pubkey, &pool)
            .await
            .unwrap();
        assert_eq!(cleared.len(), 1);
        assert!(cleared[0].muted_until.is_none());
    }

    #[tokio::test]
    async fn test_mark_left_and_removed() {
        let (pool, _dir) = test_pool().await;
        let pubkey = test_pubkey();

        // Test mark_left_atomic
        let group_id1 = GroupId::from_slice(&[60; 32]);
        let (ag, _) = AccountGroup::find_or_create(&pubkey, &group_id1, None, &pool)
            .await
            .unwrap();
        let left = ag.mark_left_atomic(&pool).await.unwrap();
        assert!(left.is_some());
        let left = left.unwrap();
        assert!(left.removed_at.is_some());
        assert!(left.self_removed);

        // Idempotent
        let again = left.mark_left_atomic(&pool).await.unwrap();
        assert!(again.is_none());

        // Test mark_removed_atomic
        let group_id2 = GroupId::from_slice(&[61; 32]);
        let (ag2, _) = AccountGroup::find_or_create(&pubkey, &group_id2, None, &pool)
            .await
            .unwrap();
        let removed = ag2.mark_removed_atomic(&pool).await.unwrap();
        assert!(removed.is_some());
        let removed = removed.unwrap();
        assert!(removed.removed_at.is_some());
        assert!(!removed.self_removed);
    }
}

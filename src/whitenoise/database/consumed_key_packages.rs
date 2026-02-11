use nostr_sdk::PublicKey;

use super::{Database, DatabaseError};

/// Represents a consumed key package tracked for delayed cleanup.
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct ConsumedKeyPackage {
    pub id: i64,
    pub account_pubkey: String,
    pub key_package_hash_ref: Vec<u8>,
    pub consumed_at: i64,
}

impl ConsumedKeyPackage {
    /// Records a consumed key package for later cleanup.
    ///
    /// Uses INSERT OR REPLACE so that if the same key package is consumed by
    /// multiple welcomes, the timestamp is updated to the latest consumption.
    pub(crate) async fn track(
        account_pubkey: &PublicKey,
        key_package_hash_ref: &[u8],
        database: &Database,
    ) -> Result<(), DatabaseError> {
        sqlx::query(
            "INSERT OR REPLACE INTO consumed_key_packages (account_pubkey, key_package_hash_ref, consumed_at)
             VALUES (?, ?, unixepoch())",
        )
        .bind(account_pubkey.to_hex())
        .bind(key_package_hash_ref)
        .execute(&database.pool)
        .await?;

        tracing::debug!(
            target: "whitenoise::database::consumed_key_packages",
            "Tracked consumed key package for account {}",
            account_pubkey.to_hex()
        );

        Ok(())
    }

    /// Returns all consumed key packages for an account that are eligible for cleanup.
    ///
    /// A package is eligible when all consumed packages for the account have a
    /// `consumed_at` timestamp older than `quiet_period_secs` seconds ago.
    /// This ensures we wait for the burst of welcomes to finish before cleaning up.
    pub(crate) async fn find_eligible_for_cleanup(
        account_pubkey: &PublicKey,
        quiet_period_secs: i64,
        database: &Database,
    ) -> Result<Vec<ConsumedKeyPackage>, DatabaseError> {
        // Only return entries if the most recent consumed_at for this account
        // is older than the quiet period. This means no new welcomes have
        // come in recently.
        let rows = sqlx::query_as::<_, (i64, String, Vec<u8>, i64)>(
            "SELECT id, account_pubkey, key_package_hash_ref, consumed_at
             FROM consumed_key_packages
             WHERE account_pubkey = ?
               AND NOT EXISTS (
                   SELECT 1 FROM consumed_key_packages
                   WHERE account_pubkey = ?
                     AND consumed_at > unixepoch() - ?
               )",
        )
        .bind(account_pubkey.to_hex())
        .bind(account_pubkey.to_hex())
        .bind(quiet_period_secs)
        .fetch_all(&database.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, account_pubkey, key_package_hash_ref, consumed_at)| Self {
                    id,
                    account_pubkey,
                    key_package_hash_ref,
                    consumed_at,
                },
            )
            .collect())
    }

    /// Removes a consumed key package record after successful cleanup.
    pub(crate) async fn delete(id: i64, database: &Database) -> Result<(), DatabaseError> {
        sqlx::query("DELETE FROM consumed_key_packages WHERE id = ?")
            .bind(id)
            .execute(&database.pool)
            .await?;
        Ok(())
    }

    /// Removes all consumed key package records for an account.
    #[allow(dead_code)]
    pub(crate) async fn delete_all_for_account(
        account_pubkey: &PublicKey,
        database: &Database,
    ) -> Result<u64, DatabaseError> {
        let result = sqlx::query("DELETE FROM consumed_key_packages WHERE account_pubkey = ?")
            .bind(account_pubkey.to_hex())
            .execute(&database.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::Keys;
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    async fn setup_test_db() -> Database {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE consumed_key_packages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey TEXT NOT NULL,
                key_package_hash_ref BLOB NOT NULL,
                consumed_at INTEGER NOT NULL,
                UNIQUE(account_pubkey, key_package_hash_ref)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        Database {
            pool,
            path: std::path::PathBuf::from(":memory:"),
            last_connected: std::time::SystemTime::now(),
        }
    }

    #[tokio::test]
    async fn test_track_consumed_key_package() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();
        let hash_ref = vec![1, 2, 3, 4, 5];

        let result = ConsumedKeyPackage::track(&pubkey, &hash_ref, &db).await;
        assert!(result.is_ok());

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM consumed_key_packages WHERE account_pubkey = ?")
                .bind(pubkey.to_hex())
                .fetch_one(&db.pool)
                .await
                .unwrap();

        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn test_track_duplicate_updates_timestamp() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();
        let hash_ref = vec![1, 2, 3, 4, 5];

        ConsumedKeyPackage::track(&pubkey, &hash_ref, &db)
            .await
            .unwrap();
        ConsumedKeyPackage::track(&pubkey, &hash_ref, &db)
            .await
            .unwrap();

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM consumed_key_packages WHERE account_pubkey = ?")
                .bind(pubkey.to_hex())
                .fetch_one(&db.pool)
                .await
                .unwrap();

        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn test_find_eligible_nothing_when_recent() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();

        ConsumedKeyPackage::track(&pubkey, &[1, 2, 3], &db)
            .await
            .unwrap();

        let eligible = ConsumedKeyPackage::find_eligible_for_cleanup(&pubkey, 30, &db)
            .await
            .unwrap();

        assert!(
            eligible.is_empty(),
            "Recently consumed packages should not be eligible"
        );
    }

    #[tokio::test]
    async fn test_find_eligible_returns_old_packages() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();
        let hash_ref: &[u8] = &[1, 2, 3];

        sqlx::query(
            "INSERT INTO consumed_key_packages (account_pubkey, key_package_hash_ref, consumed_at)
             VALUES (?, ?, unixepoch() - 60)",
        )
        .bind(pubkey.to_hex())
        .bind(hash_ref)
        .execute(&db.pool)
        .await
        .unwrap();

        let eligible = ConsumedKeyPackage::find_eligible_for_cleanup(&pubkey, 30, &db)
            .await
            .unwrap();

        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].key_package_hash_ref, hash_ref);
    }

    #[tokio::test]
    async fn test_find_eligible_blocked_by_recent_package() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();

        sqlx::query(
            "INSERT INTO consumed_key_packages (account_pubkey, key_package_hash_ref, consumed_at)
             VALUES (?, ?, unixepoch() - 60)",
        )
        .bind(pubkey.to_hex())
        .bind(&[1u8, 2, 3] as &[u8])
        .execute(&db.pool)
        .await
        .unwrap();

        ConsumedKeyPackage::track(&pubkey, &[4, 5, 6], &db)
            .await
            .unwrap();

        let eligible = ConsumedKeyPackage::find_eligible_for_cleanup(&pubkey, 30, &db)
            .await
            .unwrap();

        assert!(
            eligible.is_empty(),
            "No packages should be eligible when a recent one exists"
        );
    }

    #[tokio::test]
    async fn test_delete_removes_record() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();

        ConsumedKeyPackage::track(&pubkey, &[1, 2, 3], &db)
            .await
            .unwrap();

        let row: (i64,) =
            sqlx::query_as("SELECT id FROM consumed_key_packages WHERE account_pubkey = ?")
                .bind(pubkey.to_hex())
                .fetch_one(&db.pool)
                .await
                .unwrap();

        ConsumedKeyPackage::delete(row.0, &db).await.unwrap();

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM consumed_key_packages WHERE account_pubkey = ?")
                .bind(pubkey.to_hex())
                .fetch_one(&db.pool)
                .await
                .unwrap();

        assert_eq!(count.0, 0);
    }

    #[tokio::test]
    async fn test_delete_all_for_account() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();

        ConsumedKeyPackage::track(&pubkey, &[1, 2, 3], &db)
            .await
            .unwrap();
        ConsumedKeyPackage::track(&pubkey, &[4, 5, 6], &db)
            .await
            .unwrap();

        let deleted = ConsumedKeyPackage::delete_all_for_account(&pubkey, &db)
            .await
            .unwrap();

        assert_eq!(deleted, 2);
    }

    #[tokio::test]
    async fn test_different_accounts_independent() {
        let db = setup_test_db().await;
        let pubkey1 = Keys::generate().public_key();
        let pubkey2 = Keys::generate().public_key();

        sqlx::query(
            "INSERT INTO consumed_key_packages (account_pubkey, key_package_hash_ref, consumed_at)
             VALUES (?, ?, unixepoch() - 60)",
        )
        .bind(pubkey1.to_hex())
        .bind(&[1u8, 2, 3] as &[u8])
        .execute(&db.pool)
        .await
        .unwrap();

        ConsumedKeyPackage::track(&pubkey2, &[4, 5, 6], &db)
            .await
            .unwrap();

        let eligible1 = ConsumedKeyPackage::find_eligible_for_cleanup(&pubkey1, 30, &db)
            .await
            .unwrap();
        assert_eq!(eligible1.len(), 1);

        let eligible2 = ConsumedKeyPackage::find_eligible_for_cleanup(&pubkey2, 30, &db)
            .await
            .unwrap();
        assert!(eligible2.is_empty());
    }
}

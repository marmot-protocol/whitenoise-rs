//! Per-account repository for published key package lifecycle tracking.
//!
//! Wraps the existing [`PublishedKeyPackage`] DB functions so that callers do not
//! need to thread an `account_pubkey` argument through every call — the pubkey
//! is baked in at construction time.

use std::sync::Arc;

use nostr_sdk::PublicKey;

use crate::whitenoise::database::Database;
use crate::whitenoise::database::published_key_packages::PublishedKeyPackage;
use crate::whitenoise::error::Result;

/// Repository for published key package lifecycle records scoped to a single account.
#[derive(Clone, Debug)]
pub struct PublishedKeyPackagesRepo {
    account_pubkey: PublicKey,
    db: Arc<Database>,
}

impl PublishedKeyPackagesRepo {
    /// Construct a new [`PublishedKeyPackagesRepo`] for `account_pubkey`.
    pub(crate) fn new(account_pubkey: PublicKey, db: Arc<Database>) -> Self {
        Self { account_pubkey, db }
    }

    /// Record a successfully published key package for lifecycle tracking.
    ///
    /// Delegates to `PublishedKeyPackage::create`.
    pub async fn create(&self, hash_ref: &[u8], event_id: &str) -> Result<()> {
        Ok(PublishedKeyPackage::create(&self.account_pubkey, hash_ref, event_id, &self.db).await?)
    }

    /// Look up a published key package by its event ID.
    ///
    /// Delegates to `PublishedKeyPackage::find_by_event_id`.
    pub async fn find_by_event_id(&self, event_id: &str) -> Result<Option<PublishedKeyPackage>> {
        Ok(PublishedKeyPackage::find_by_event_id(&self.account_pubkey, event_id, &self.db).await?)
    }

    /// Mark a published key package as consumed (used by a Welcome).
    ///
    /// Returns `false` if no matching row exists or key material is already deleted.
    ///
    /// Delegates to `PublishedKeyPackage::mark_consumed`.
    pub async fn mark_consumed(&self, event_id: &str) -> Result<bool> {
        Ok(PublishedKeyPackage::mark_consumed(&self.account_pubkey, event_id, &self.db).await?)
    }

    /// Return all published key packages eligible for key material cleanup.
    ///
    /// A package is eligible when all consumed packages for this account have
    /// `consumed_at` older than `quiet_period_secs`.
    ///
    /// Delegates to `PublishedKeyPackage::find_eligible_for_cleanup`.
    pub async fn find_eligible_for_cleanup(
        &self,
        quiet_period_secs: i64,
    ) -> Result<Vec<PublishedKeyPackage>> {
        Ok(PublishedKeyPackage::find_eligible_for_cleanup(
            &self.account_pubkey,
            quiet_period_secs,
            &self.db,
        )
        .await?)
    }

    /// Mark a published key package's key material as deleted.
    ///
    /// Delegates to `PublishedKeyPackage::mark_key_material_deleted`.
    pub async fn mark_key_material_deleted(&self, id: i64) -> Result<()> {
        Ok(PublishedKeyPackage::mark_key_material_deleted(id, &self.db).await?)
    }

    /// Backdate `consumed_at` into the past for testing cleanup eligibility.
    #[cfg(feature = "integration-tests")]
    pub async fn backdate_consumed_at(&self, event_id: &str, age_secs: i64) -> Result<()> {
        sqlx::query(
            "UPDATE published_key_packages SET consumed_at = unixepoch() - ?
             WHERE account_pubkey = ? AND event_id = ?",
        )
        .bind(age_secs)
        .bind(self.account_pubkey.to_hex())
        .bind(event_id)
        .execute(&self.db.pool)
        .await
        .map_err(crate::whitenoise::database::DatabaseError::Sqlx)?;
        Ok(())
    }
}

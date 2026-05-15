//! Per-account repository for published key package lifecycle tracking.

use std::sync::Arc;

use crate::whitenoise::database::account_db::AccountDatabase;
use crate::whitenoise::database::published_key_packages::PublishedKeyPackage;
use crate::whitenoise::error::Result;

/// Repository for published key package lifecycle records scoped to a single account.
#[derive(Clone, Debug)]
pub struct PublishedKeyPackagesRepo {
    db: Arc<AccountDatabase>,
}

impl PublishedKeyPackagesRepo {
    pub(crate) fn new(db: Arc<AccountDatabase>) -> Self {
        Self { db }
    }

    /// Record a successfully published key package for lifecycle tracking.
    pub async fn create(
        &self,
        hash_ref: &[u8],
        event_id: &str,
        kind: nostr_sdk::Kind,
        d_tag: Option<&str>,
    ) -> Result<()> {
        Ok(PublishedKeyPackage::create(&self.db, hash_ref, event_id, kind, d_tag).await?)
    }

    /// Look up a published key package by its event ID.
    pub async fn find_by_event_id(&self, event_id: &str) -> Result<Option<PublishedKeyPackage>> {
        Ok(PublishedKeyPackage::find_by_event_id(&self.db, event_id).await?)
    }

    /// Look up all published key packages sharing the same hash reference.
    pub async fn find_by_hash_ref(&self, hash_ref: &[u8]) -> Result<Vec<PublishedKeyPackage>> {
        Ok(PublishedKeyPackage::find_by_hash_ref(&self.db, hash_ref).await?)
    }

    /// Return the most recently inserted row for a given event kind, if any.
    ///
    /// Canonical-slot consumers read both `d_tag` and `created_at` from
    /// this single row. Folding the lookup into one query avoids the
    /// implication that the d-tag and the insert timestamp could come
    /// from two different prior publishes.
    pub async fn find_latest_by_kind(
        &self,
        kind: nostr_sdk::Kind,
    ) -> Result<Option<PublishedKeyPackage>> {
        Ok(PublishedKeyPackage::find_latest_by_kind(&self.db, kind).await?)
    }

    /// Mark a published key package as consumed (used by a Welcome).
    pub async fn mark_consumed(&self, event_id: &str) -> Result<bool> {
        Ok(PublishedKeyPackage::mark_consumed(&self.db, event_id).await?)
    }

    /// Return all published key packages eligible for key material cleanup.
    pub async fn find_eligible_for_cleanup(
        &self,
        quiet_period_secs: i64,
    ) -> Result<Vec<PublishedKeyPackage>> {
        Ok(PublishedKeyPackage::find_eligible_for_cleanup(&self.db, quiet_period_secs).await?)
    }

    /// Mark a published key package's key material as deleted by hash ref.
    pub async fn mark_key_material_deleted_by_hash_ref(&self, hash_ref: &[u8]) -> Result<()> {
        PublishedKeyPackage::mark_key_material_deleted_by_hash_ref(&self.db, hash_ref).await?;
        Ok(())
    }

    /// Mark a published key package's key material as deleted by row id.
    pub async fn mark_key_material_deleted(&self, id: i64) -> Result<()> {
        Ok(PublishedKeyPackage::mark_key_material_deleted(&self.db, id).await?)
    }

    /// Backdate `consumed_at` into the past for testing cleanup eligibility.
    ///
    /// Mirrors `PublishedKeyPackage::mark_consumed` by updating ALL rows
    /// sharing the same `key_package_hash_ref` as the row matching `event_id`.
    /// Dual-published kind:30443/kind:443 twins must stay in sync — otherwise
    /// the un-backdated twin's recent `consumed_at` keeps tripping the
    /// `NOT EXISTS` clause in `find_eligible_for_cleanup`.
    #[cfg(feature = "integration-tests")]
    pub async fn backdate_consumed_at(&self, event_id: &str, age_secs: i64) -> Result<()> {
        sqlx::query(
            "UPDATE published_key_packages
             SET consumed_at = unixepoch() - ?
             WHERE key_package_hash_ref = (
                 SELECT key_package_hash_ref FROM published_key_packages WHERE event_id = ?
             )",
        )
        .bind(age_secs)
        .bind(event_id)
        .execute(&self.db.inner.pool)
        .await
        .map_err(crate::whitenoise::database::DatabaseError::Sqlx)?;
        Ok(())
    }
}

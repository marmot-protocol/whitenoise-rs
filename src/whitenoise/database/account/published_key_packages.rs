//! Per-account repository for published key package lifecycle tracking.

use std::sync::Arc;

use crate::whitenoise::database::account_db::AccountDatabase;
use crate::whitenoise::database::published_key_packages::{
    PublishedKeyPackage, PublishedKeyPackageProtocolData,
};
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

    /// Record a successfully published key package with protocol metadata.
    pub async fn create_with_protocol_data(
        &self,
        hash_ref: &[u8],
        event_id: &str,
        kind: nostr_sdk::Kind,
        d_tag: Option<&str>,
        protocol_data: PublishedKeyPackageProtocolData,
    ) -> Result<()> {
        Ok(PublishedKeyPackage::create_with_protocol_data(
            &self.db,
            hash_ref,
            event_id,
            kind,
            d_tag,
            protocol_data,
        )
        .await?)
    }

    /// Look up a published key package by its event ID.
    pub async fn find_by_event_id(&self, event_id: &str) -> Result<Option<PublishedKeyPackage>> {
        Ok(PublishedKeyPackage::find_by_event_id(&self.db, event_id).await?)
    }

    /// Look up all published key packages sharing the same hash reference.
    pub async fn find_by_hash_ref(&self, hash_ref: &[u8]) -> Result<Vec<PublishedKeyPackage>> {
        Ok(PublishedKeyPackage::find_by_hash_ref(&self.db, hash_ref).await?)
    }

    #[cfg(feature = "integration-tests")]
    pub async fn find_consumed(&self) -> Result<Vec<PublishedKeyPackage>> {
        Ok(PublishedKeyPackage::find_consumed(&self.db).await?)
    }

    /// Return the most recently inserted row for a given event kind, if any.
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

    /// Returns true if any consumed key package still falls within the quiet period.
    pub async fn has_consumed_since(&self, quiet_period_secs: i64) -> Result<bool> {
        Ok(PublishedKeyPackage::has_consumed_since(&self.db, quiet_period_secs).await?)
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
    /// Hash-ref siblings must stay in sync, otherwise one recent
    /// `consumed_at` keeps tripping the `NOT EXISTS` clause in
    /// `find_eligible_for_cleanup`.
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

#[cfg(test)]
mod tests {
    use nostr_sdk::{Keys, Kind};
    use tempfile::TempDir;

    use super::*;

    async fn setup() -> (PublishedKeyPackagesRepo, TempDir) {
        let dir = TempDir::new().unwrap();
        let pubkey = Keys::generate().public_key();
        let db = Arc::new(
            AccountDatabase::new(pubkey, dir.path().join("acct.db"))
                .await
                .unwrap(),
        );
        // Matches the project-wide account-DB test pattern: stamp the current
        // table shape directly because `AccountDatabase::new` uses
        // `open_without_migrations` and running the full migration timeline
        // here would require wiring a shared pool.
        sqlx::query("DROP TABLE IF EXISTS published_key_packages")
            .execute(&db.inner.pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE published_key_packages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                key_package_hash_ref BLOB NOT NULL,
                key_package_ref BLOB NULL,
                key_package_content TEXT NULL,
                event_id TEXT NOT NULL UNIQUE,
                kind INTEGER NOT NULL DEFAULT 30443,
                d_tag TEXT NULL,
                app_components TEXT NOT NULL DEFAULT '[]',
                package_version INTEGER NOT NULL DEFAULT 1,
                package_role TEXT NOT NULL DEFAULT 'legacy'
                    CHECK (package_role IN ('legacy', 'last_resort', 'rotated')),
                consumed_at INTEGER,
                key_material_deleted INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (unixepoch())
            )",
        )
        .execute(&db.inner.pool)
        .await
        .unwrap();
        (PublishedKeyPackagesRepo::new(db), dir)
    }

    /// Regression: `mark_key_material_deleted(id)` clears the flag for a single
    /// row identified by primary key, NOT by hash_ref. The companion
    /// `mark_key_material_deleted_by_hash_ref` is the hash-scoped variant.
    /// This is the contrapositive of PR #822: a future refactor that
    /// "fixes consistency" by accidentally hash_ref-scoping the by-id variant
    /// must trip this test.
    #[tokio::test]
    async fn mark_key_material_deleted_by_id_flips_one_row_only() {
        let (repo, _dir) = setup().await;
        let hash_ref = b"hash_id_scoped";

        repo.create(hash_ref, "evt_a_id", Kind::Custom(30443), Some("d"))
            .await
            .unwrap();
        repo.create(hash_ref, "evt_b_id", Kind::Custom(30443), Some("d-2"))
            .await
            .unwrap();

        // Resolve the canonical row's id and flip only it.
        let canonical = repo
            .find_by_event_id("evt_a_id")
            .await
            .unwrap()
            .expect("must exist");
        repo.mark_key_material_deleted(canonical.id).await.unwrap();

        let siblings = repo.find_by_hash_ref(hash_ref).await.unwrap();
        assert_eq!(siblings.len(), 2);
        let deleted_events: Vec<String> = siblings
            .iter()
            .filter(|t| t.key_material_deleted)
            .map(|t| t.event_id.clone())
            .collect();
        assert_eq!(
            deleted_events,
            vec!["evt_a_id".to_string()],
            "by-id variant must only flip the canonical row"
        );
    }

    #[tokio::test]
    async fn has_consumed_since_tracks_recent_non_deleted_rows() {
        let (repo, _dir) = setup().await;
        let hash_ref = b"recent_hash";

        repo.create(
            hash_ref,
            "evt_recent",
            Kind::Custom(30443),
            Some("recent-d"),
        )
        .await
        .unwrap();
        assert!(!repo.has_consumed_since(30).await.unwrap());

        repo.mark_consumed("evt_recent").await.unwrap();
        assert!(repo.has_consumed_since(30).await.unwrap());

        repo.mark_key_material_deleted_by_hash_ref(hash_ref)
            .await
            .unwrap();
        assert!(!repo.has_consumed_since(30).await.unwrap());
    }

    #[tokio::test]
    async fn find_eligible_for_cleanup_delegates_quiet_period_filter() {
        let (repo, _dir) = setup().await;
        let hash_ref = b"eligible_hash";

        repo.create(
            hash_ref,
            "evt_eligible",
            Kind::Custom(30443),
            Some("eligible-d"),
        )
        .await
        .unwrap();
        repo.mark_consumed("evt_eligible").await.unwrap();
        sqlx::query(
            "UPDATE published_key_packages
             SET consumed_at = unixepoch() - 60
             WHERE event_id = ?",
        )
        .bind("evt_eligible")
        .execute(&repo.db.inner.pool)
        .await
        .unwrap();

        let eligible = repo.find_eligible_for_cleanup(30).await.unwrap();

        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].event_id, "evt_eligible");
    }
}

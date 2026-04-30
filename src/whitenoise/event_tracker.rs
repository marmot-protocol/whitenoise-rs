use std::sync::{Arc, Weak};

use async_trait::async_trait;
use nostr_sdk::prelude::*;

use crate::perf_instrument;
use crate::whitenoise::{
    Whitenoise,
    database::{Database, processed_events::ProcessedEvent, published_events::PublishedEvent},
    utils::timestamp_to_datetime,
};

/// Trait for handling event tracking operations
#[async_trait]
pub trait EventTracker: Send + Sync {
    /// Track that an account published a specific event
    async fn track_published_event(
        &self,
        event_id: &EventId,
        pubkey: &PublicKey,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Check if the account was the publisher of a specific event
    async fn account_published_event(
        &self,
        event_id: &EventId,
        pubkey: &PublicKey,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;

    /// Check if we published a given event, regardless of account
    async fn global_published_event(
        &self,
        event_id: &EventId,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;

    /// Track that we processed a specific event for an account
    async fn track_processed_account_event(
        &self,
        event: &Event,
        pubkey: &PublicKey,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Check if we already processed a specific event for an account
    async fn already_processed_account_event(
        &self,
        event_id: &EventId,
        pubkey: &PublicKey,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;

    /// Track that we processed a specific global event
    async fn track_processed_global_event(
        &self,
        event: &Event,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Check if we already processed a specific global event
    async fn already_processed_global_event(
        &self,
        event_id: &EventId,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;
}

/// No-op implementation that doesn't track events
pub struct NoEventTracker;

#[async_trait]
impl EventTracker for NoEventTracker {
    async fn track_published_event(
        &self,
        _event_id: &EventId,
        _pubkey: &PublicKey,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    async fn account_published_event(
        &self,
        _event_id: &EventId,
        _pubkey: &PublicKey,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        Ok(false)
    }

    async fn global_published_event(
        &self,
        _event_id: &EventId,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        Ok(false)
    }

    async fn track_processed_account_event(
        &self,
        _event: &Event,
        _pubkey: &PublicKey,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    async fn already_processed_account_event(
        &self,
        _event_id: &EventId,
        _pubkey: &PublicKey,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        Ok(false)
    }

    async fn track_processed_global_event(
        &self,
        _event: &Event,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    async fn already_processed_global_event(
        &self,
        _event_id: &EventId,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        Ok(false)
    }
}

/// Database-backed event tracker.
///
/// Account-scoped operations route through registered sessions on
/// `Whitenoise::account_manager` (the per-account `published_events` and
/// `processed_events` tables live in each account's per-account DB after
/// Phase 18c). Global-scoped operations use the shared DB directly. Holds a
/// `Weak<Whitenoise>` to avoid the cycle
/// `Whitenoise -> SharedServices -> EventTracker -> Whitenoise`.
pub struct WhitenoiseEventTracker {
    database: Arc<Database>,
    whitenoise: Weak<Whitenoise>,
}

impl WhitenoiseEventTracker {
    pub fn new(database: Arc<Database>, whitenoise: Weak<Whitenoise>) -> Self {
        Self {
            database,
            whitenoise,
        }
    }

    /// Convenience helper for tests and bootstrap paths that don't have a
    /// `Whitenoise` yet. Account-scoped DB ops will silently no-op until a
    /// session can be located via [`Self::set_whitenoise`] equivalent paths;
    /// callers should prefer [`Self::new`] in production code.
    #[cfg(test)]
    pub fn detached(database: Arc<Database>) -> Self {
        Self {
            database,
            whitenoise: Weak::new(),
        }
    }

    fn with_whitenoise<F, T>(&self, f: F) -> Option<T>
    where
        F: FnOnce(Arc<Whitenoise>) -> T,
    {
        self.whitenoise.upgrade().map(f)
    }

    fn account_db_for(
        &self,
        pubkey: &PublicKey,
    ) -> Option<Arc<crate::whitenoise::database::account_db::AccountDatabase>> {
        self.with_whitenoise(|wn| wn.session(pubkey).map(|s| s.account_db.clone()))
            .flatten()
    }
}

#[async_trait]
impl EventTracker for WhitenoiseEventTracker {
    #[perf_instrument("event_tracker")]
    async fn track_published_event(
        &self,
        event_id: &EventId,
        pubkey: &PublicKey,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(account_db) = self.account_db_for(pubkey) {
            PublishedEvent::create(event_id, &account_db)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        } else {
            tracing::warn!(
                target: "whitenoise::event_tracker",
                pubkey = %pubkey,
                event_id = %event_id,
                "track_published_event: no session for account; skipping"
            );
        }
        Ok(())
    }

    #[perf_instrument("event_tracker")]
    async fn account_published_event(
        &self,
        event_id: &EventId,
        pubkey: &PublicKey,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        match self.account_db_for(pubkey) {
            Some(account_db) => PublishedEvent::exists(event_id, &account_db)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
            None => Ok(false),
        }
    }

    #[perf_instrument("event_tracker")]
    async fn global_published_event(
        &self,
        event_id: &EventId,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        // Iterate every active session's per-account DB. Number of logged-in
        // accounts is small (1–3); the iteration cost is negligible.
        let Some(wn) = self.whitenoise.upgrade() else {
            return Ok(false);
        };
        let sessions: Vec<_> = wn
            .account_manager
            .sessions_iter()
            .map(|s| s.account_db.clone())
            .collect();
        for account_db in sessions {
            let exists = PublishedEvent::exists(event_id, &account_db)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            if exists {
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[perf_instrument("event_tracker")]
    async fn track_processed_account_event(
        &self,
        event: &Event,
        pubkey: &PublicKey,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(account_db) = self.account_db_for(pubkey) {
            ProcessedEvent::create_for_account(
                &event.id,
                Some(timestamp_to_datetime(event.created_at)?),
                Some(event.kind),
                Some(&event.pubkey),
                &account_db,
            )
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        } else {
            tracing::warn!(
                target: "whitenoise::event_tracker",
                pubkey = %pubkey,
                event_id = %event.id,
                "track_processed_account_event: no session for account; skipping"
            );
        }
        Ok(())
    }

    #[perf_instrument("event_tracker")]
    async fn already_processed_account_event(
        &self,
        event_id: &EventId,
        pubkey: &PublicKey,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        match self.account_db_for(pubkey) {
            Some(account_db) => ProcessedEvent::exists_for_account(event_id, &account_db)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
            None => Ok(false),
        }
    }

    #[perf_instrument("event_tracker")]
    async fn track_processed_global_event(
        &self,
        event: &Event,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        ProcessedEvent::create_global(
            &event.id,
            Some(timestamp_to_datetime(event.created_at)?),
            Some(event.kind),
            Some(&event.pubkey),
            &self.database,
        )
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    #[perf_instrument("event_tracker")]
    async fn already_processed_global_event(
        &self,
        event_id: &EventId,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        ProcessedEvent::exists_global(event_id, &self.database)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::Keys;
    use tempfile::TempDir;

    async fn create_test_event() -> Event {
        let keys = Keys::generate();
        EventBuilder::text_note("test content")
            .sign(&keys)
            .await
            .unwrap()
    }

    async fn create_test_database() -> (Arc<Database>, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.sqlite");
        let database = Arc::new(Database::new(db_path).await.unwrap());
        (database, temp_dir)
    }

    mod no_event_tracker {
        use super::*;

        #[tokio::test]
        async fn all_methods_return_expected_noop_values() {
            let tracker = NoEventTracker;
            let event = create_test_event().await;

            assert!(
                tracker
                    .track_published_event(&event.id, &event.pubkey)
                    .await
                    .is_ok()
            );
            assert!(
                tracker
                    .track_processed_account_event(&event, &event.pubkey)
                    .await
                    .is_ok()
            );
            assert!(tracker.track_processed_global_event(&event).await.is_ok());

            assert!(
                !tracker
                    .account_published_event(&event.id, &event.pubkey)
                    .await
                    .unwrap()
            );
            assert!(!tracker.global_published_event(&event.id).await.unwrap());
            assert!(
                !tracker
                    .already_processed_account_event(&event.id, &event.pubkey)
                    .await
                    .unwrap()
            );
            assert!(
                !tracker
                    .already_processed_global_event(&event.id)
                    .await
                    .unwrap()
            );
        }
    }

    mod whitenoise_event_tracker {
        use super::*;

        #[tokio::test]
        async fn construction_works() {
            let (database, _temp_dir) = create_test_database().await;
            let tracker = WhitenoiseEventTracker::detached(database);
            let _ = tracker;
        }

        /// Without a `Whitenoise` upgrade, account-scoped DB ops are no-ops.
        #[tokio::test]
        async fn detached_tracker_account_ops_are_noops() {
            let (database, _temp_dir) = create_test_database().await;
            let tracker = WhitenoiseEventTracker::detached(database);
            let event = create_test_event().await;

            assert!(
                tracker
                    .track_published_event(&event.id, &event.pubkey)
                    .await
                    .is_ok()
            );
            assert!(
                !tracker
                    .account_published_event(&event.id, &event.pubkey)
                    .await
                    .unwrap()
            );
            assert!(!tracker.global_published_event(&event.id).await.unwrap());
            assert!(
                !tracker
                    .already_processed_account_event(&event.id, &event.pubkey)
                    .await
                    .unwrap()
            );
        }

        #[tokio::test]
        async fn track_and_check_global_processed_event() {
            let (database, _temp_dir) = create_test_database().await;
            let tracker = WhitenoiseEventTracker::detached(database);
            let event = create_test_event().await;

            assert!(
                !tracker
                    .already_processed_global_event(&event.id)
                    .await
                    .unwrap()
            );

            tracker.track_processed_global_event(&event).await.unwrap();

            assert!(
                tracker
                    .already_processed_global_event(&event.id)
                    .await
                    .unwrap()
            );
        }
    }
}

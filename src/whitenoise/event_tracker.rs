use std::sync::Arc;

use async_trait::async_trait;
use nostr_sdk::prelude::*;

use crate::perf_span;
use crate::whitenoise::{
    accounts::Account,
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
        Ok(()) // Do nothing
    }

    async fn account_published_event(
        &self,
        _event_id: &EventId,
        _pubkey: &PublicKey,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        Ok(false) // Do nothing
    }

    async fn global_published_event(
        &self,
        _event_id: &EventId,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        Ok(false) // Do nothing
    }

    async fn track_processed_account_event(
        &self,
        _event: &Event,
        _pubkey: &PublicKey,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(()) // Do nothing
    }

    async fn already_processed_account_event(
        &self,
        _event_id: &EventId,
        _pubkey: &PublicKey,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        Ok(false) // Do nothing
    }

    async fn track_processed_global_event(
        &self,
        _event: &Event,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(()) // Do nothing
    }

    async fn already_processed_global_event(
        &self,
        _event_id: &EventId,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        Ok(false) // Do nothing
    }
}

/// Database-backed event tracker with dependency injection
pub struct WhitenoiseEventTracker {
    database: Arc<Database>,
}

impl WhitenoiseEventTracker {
    pub fn new(database: Arc<Database>) -> Self {
        Self { database }
    }
}

#[async_trait]
impl EventTracker for WhitenoiseEventTracker {
    async fn track_published_event(
        &self,
        event_id: &EventId,
        pubkey: &PublicKey,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _span = perf_span!("event_tracker::track_published_event");
        let account = Account::find_by_pubkey(pubkey, &self.database).await?;
        let account_id = account
            .id
            .ok_or_else(|| std::io::Error::other("Account missing id"))?;
        PublishedEvent::create(event_id, account_id, &self.database)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        Ok(())
    }

    async fn account_published_event(
        &self,
        event_id: &EventId,
        pubkey: &PublicKey,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let _span = perf_span!("event_tracker::account_published_event");
        let account = Account::find_by_pubkey(pubkey, &self.database).await?;
        let account_id = account
            .id
            .ok_or_else(|| std::io::Error::other("Account missing id"))?;
        PublishedEvent::exists(event_id, Some(account_id), &self.database)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    async fn global_published_event(
        &self,
        event_id: &EventId,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let _span = perf_span!("event_tracker::global_published_event");
        PublishedEvent::exists(event_id, None, &self.database)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    async fn track_processed_account_event(
        &self,
        event: &Event,
        pubkey: &PublicKey,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _span = perf_span!("event_tracker::track_processed_account_event");
        let account = Account::find_by_pubkey(pubkey, &self.database).await?;
        let account_id = account
            .id
            .ok_or_else(|| std::io::Error::other("Account missing id"))?;
        ProcessedEvent::create(
            &event.id,
            Some(account_id),
            Some(timestamp_to_datetime(event.created_at)?),
            Some(event.kind),
            Some(&event.pubkey),
            &self.database,
        )
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    async fn already_processed_account_event(
        &self,
        event_id: &EventId,
        pubkey: &PublicKey,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let _span = perf_span!("event_tracker::already_processed_account_event");
        let account = Account::find_by_pubkey(pubkey, &self.database).await?;
        let account_id = account
            .id
            .ok_or_else(|| std::io::Error::other("Account missing id"))?;
        ProcessedEvent::exists(event_id, Some(account_id), &self.database)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    async fn track_processed_global_event(
        &self,
        event: &Event,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _span = perf_span!("event_tracker::track_processed_global_event");
        ProcessedEvent::create(
            &event.id,
            None,
            Some(timestamp_to_datetime(event.created_at)?),
            Some(event.kind),
            Some(&event.pubkey),
            &self.database,
        )
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    async fn already_processed_global_event(
        &self,
        event_id: &EventId,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let _span = perf_span!("event_tracker::already_processed_global_event");
        ProcessedEvent::exists(event_id, None, &self.database)
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

    /// Creates a test account by inserting directly into the database.
    /// This satisfies the foreign key constraints without requiring full Whitenoise setup.
    async fn create_test_account(db: &Database, pubkey: &PublicKey) {
        // Create test user first
        let now = chrono::Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO users (pubkey, metadata, created_at, updated_at) VALUES (?, '{}', ?, ?)",
        )
        .bind(pubkey.to_hex())
        .bind(now)
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();

        let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE pubkey = ?")
            .bind(pubkey.to_hex())
            .fetch_one(&db.pool)
            .await
            .unwrap();

        // Create account linked to user
        sqlx::query(
            "INSERT INTO accounts (pubkey, user_id, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(pubkey.to_hex())
        .bind(user_id)
        .bind(now)
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    mod no_event_tracker {
        use super::*;

        /// Tests all NoEventTracker methods - track operations succeed,
        /// check operations return false (no-op behavior).
        #[tokio::test]
        async fn all_methods_return_expected_noop_values() {
            let tracker = NoEventTracker;
            let event = create_test_event().await;

            // Track operations should succeed (Ok(()))
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

            // Check operations should return false (nothing tracked)
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
            let tracker = WhitenoiseEventTracker::new(database);
            let _ = tracker;
        }

        #[tokio::test]
        async fn track_and_check_global_processed_event() {
            let (database, _temp_dir) = create_test_database().await;
            let tracker = WhitenoiseEventTracker::new(database);
            let event = create_test_event().await;

            // Initially not processed
            assert!(
                !tracker
                    .already_processed_global_event(&event.id)
                    .await
                    .unwrap()
            );

            // Track it
            tracker.track_processed_global_event(&event).await.unwrap();

            // Now it should be marked as processed
            assert!(
                tracker
                    .already_processed_global_event(&event.id)
                    .await
                    .unwrap()
            );
        }

        #[tokio::test]
        async fn track_and_check_global_published_event() {
            let (database, _temp_dir) = create_test_database().await;

            // Create an account first (required for published events)
            let keys = Keys::generate();
            create_test_account(&database, &keys.public_key()).await;

            let tracker = WhitenoiseEventTracker::new(database);
            let event = EventBuilder::text_note("test").sign(&keys).await.unwrap();

            // Initially not published
            assert!(!tracker.global_published_event(&event.id).await.unwrap());

            // Track it
            tracker
                .track_published_event(&event.id, &event.pubkey)
                .await
                .unwrap();

            // Now it should be marked as published
            assert!(tracker.global_published_event(&event.id).await.unwrap());
        }

        #[tokio::test]
        async fn track_and_check_account_events() {
            let (database, _temp_dir) = create_test_database().await;

            // Create an account
            let keys = Keys::generate();
            create_test_account(&database, &keys.public_key()).await;

            let tracker = WhitenoiseEventTracker::new(database);
            let event = EventBuilder::text_note("test").sign(&keys).await.unwrap();

            // Initially not processed or published for this account
            assert!(
                !tracker
                    .already_processed_account_event(&event.id, &event.pubkey)
                    .await
                    .unwrap()
            );
            assert!(
                !tracker
                    .account_published_event(&event.id, &event.pubkey)
                    .await
                    .unwrap()
            );

            // Track processed
            tracker
                .track_processed_account_event(&event, &event.pubkey)
                .await
                .unwrap();
            assert!(
                tracker
                    .already_processed_account_event(&event.id, &event.pubkey)
                    .await
                    .unwrap()
            );

            // Track published
            tracker
                .track_published_event(&event.id, &event.pubkey)
                .await
                .unwrap();
            assert!(
                tracker
                    .account_published_event(&event.id, &event.pubkey)
                    .await
                    .unwrap()
            );
        }

        #[tokio::test]
        async fn track_published_event_fails_when_account_not_found() {
            let (database, _temp_dir) = create_test_database().await;
            let tracker = WhitenoiseEventTracker::new(database);
            let event = create_test_event().await;

            // No account created - should error
            let result = tracker
                .track_published_event(&event.id, &event.pubkey)
                .await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn account_published_event_fails_when_account_not_found() {
            let (database, _temp_dir) = create_test_database().await;
            let tracker = WhitenoiseEventTracker::new(database);
            let event = create_test_event().await;

            // No account created - should error
            let result = tracker
                .account_published_event(&event.id, &event.pubkey)
                .await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn track_processed_account_event_fails_when_account_not_found() {
            let (database, _temp_dir) = create_test_database().await;
            let tracker = WhitenoiseEventTracker::new(database);
            let event = create_test_event().await;

            // No account created - should error
            let result = tracker
                .track_processed_account_event(&event, &event.pubkey)
                .await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn already_processed_account_event_fails_when_account_not_found() {
            let (database, _temp_dir) = create_test_database().await;
            let tracker = WhitenoiseEventTracker::new(database);
            let event = create_test_event().await;

            // No account created - should error
            let result = tracker
                .already_processed_account_event(&event.id, &event.pubkey)
                .await;
            assert!(result.is_err());
        }
    }
}

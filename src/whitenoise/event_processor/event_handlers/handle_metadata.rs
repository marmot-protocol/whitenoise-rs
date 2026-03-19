use nostr_sdk::prelude::*;

use crate::{
    perf_instrument,
    whitenoise::{
        Whitenoise,
        error::{Result, WhitenoiseError},
        users::User,
    },
};

impl Whitenoise {
    #[perf_instrument("event_handlers")]
    pub async fn handle_metadata(&self, event: Event) -> Result<()> {
        let (mut user, newly_created) =
            User::find_or_create_by_pubkey(&event.pubkey, &self.database).await?;
        match Metadata::from_json(&event.content) {
            Ok(metadata) => {
                let should_update = user
                    .should_update_metadata(&event, newly_created, &self.database)
                    .await?;

                if should_update {
                    user.metadata = metadata;
                    user.mark_metadata_known_now();
                    user.save(&self.database).await?;

                    self.event_tracker
                        .track_processed_global_event(&event)
                        .await?;

                    tracing::debug!(
                        target: "whitenoise::event_processor::handle_metadata",
                        "Updated metadata for user {} with event timestamp {}",
                        event.pubkey,
                        event.created_at
                    );
                }
                Ok(())
            }
            Err(e) => {
                tracing::error!(
                    target: "whitenoise::nostr_manager::fetch_all_user_data",
                    "Failed to parse metadata for user {}: {}",
                    event.pubkey,
                    e
                );
                Err(WhitenoiseError::EventProcessor(e.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};

    use super::*;
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    async fn metadata_event(keys: &Keys, metadata: &Metadata, timestamp: Timestamp) -> Event {
        EventBuilder::metadata(metadata)
            .custom_created_at(timestamp)
            .sign(keys)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn valid_blank_kind0_marks_metadata_as_known() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let event = metadata_event(&keys, &Metadata::new(), Timestamp::now()).await;

        whitenoise.handle_metadata(event).await.unwrap();

        let user = whitenoise
            .find_user_by_pubkey(&keys.public_key())
            .await
            .unwrap();
        assert_eq!(user.metadata, Metadata::new());
        assert!(user.metadata_known_at.is_some());
    }

    #[tokio::test]
    async fn newer_valid_kind0_updates_metadata_and_known_timestamp() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let now = Timestamp::now();
        let older = metadata_event(&keys, &Metadata::new().name("older"), now).await;

        whitenoise.handle_metadata(older).await.unwrap();
        let first_user = whitenoise
            .find_user_by_pubkey(&keys.public_key())
            .await
            .unwrap();
        let first_known_at = first_user.metadata_known_at.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let newer_timestamp = Timestamp::from(now.as_secs() + 60);
        let newer = metadata_event(&keys, &Metadata::new().name("newer"), newer_timestamp).await;
        whitenoise.handle_metadata(newer).await.unwrap();

        let updated_user = whitenoise
            .find_user_by_pubkey(&keys.public_key())
            .await
            .unwrap();
        let updated_known_at = updated_user.metadata_known_at.unwrap();

        assert_eq!(updated_user.metadata.name, Some("newer".to_string()));
        assert!(updated_known_at >= first_known_at);
    }

    #[tokio::test]
    async fn older_valid_kind0_does_not_overwrite_newer_processed_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let now = Utc::now();
        let newer_timestamp = Timestamp::from(now.timestamp() as u64);
        let older_timestamp = Timestamp::from((now - Duration::minutes(5)).timestamp() as u64);

        let newer = metadata_event(&keys, &Metadata::new().name("newer"), newer_timestamp).await;
        whitenoise.handle_metadata(newer).await.unwrap();

        let first_user = whitenoise
            .find_user_by_pubkey(&keys.public_key())
            .await
            .unwrap();
        let first_known_at = first_user.metadata_known_at;

        let older = metadata_event(&keys, &Metadata::new().name("older"), older_timestamp).await;
        whitenoise.handle_metadata(older).await.unwrap();

        let user = whitenoise
            .find_user_by_pubkey(&keys.public_key())
            .await
            .unwrap();
        assert_eq!(user.metadata.name, Some("newer".to_string()));
        assert_eq!(user.metadata_known_at, first_known_at);
    }
}

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

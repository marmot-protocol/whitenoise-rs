use crate::whitenoise::user_search::{SearchUpdateTrigger, UserSearchUpdate};

/// Collects all search updates from a broadcast receiver until SearchCompleted or Error.
///
/// Returns the collected updates for assertion by the calling test case.
/// Times out after 30 seconds to prevent hanging on broken searches.
pub async fn collect_search_updates(
    mut rx: tokio::sync::broadcast::Receiver<UserSearchUpdate>,
) -> Vec<UserSearchUpdate> {
    let mut updates = Vec::new();
    let timeout = tokio::time::Duration::from_secs(30);

    let result = tokio::time::timeout(timeout, async {
        loop {
            match rx.recv().await {
                Ok(update) => {
                    let is_terminal = matches!(
                        update.trigger,
                        SearchUpdateTrigger::SearchCompleted { .. }
                            | SearchUpdateTrigger::Error { .. }
                    );
                    updates.push(update);
                    if is_terminal {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Search update receiver lagged by {n} messages");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
    .await;

    if result.is_err() {
        tracing::warn!("Search update collection timed out after {:?}", timeout);
    }

    updates
}

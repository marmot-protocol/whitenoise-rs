use nostr_sdk::PublicKey;

use crate::whitenoise::user_search::{SearchUpdateTrigger, UserSearchUpdate};

/// Collects all search updates from a broadcast receiver until SearchCompleted or Error.
///
/// Returns the collected updates for assertion by the calling test case.
/// Times out after 30 seconds to prevent hanging on broken searches.
pub async fn collect_search_updates(
    rx: tokio::sync::broadcast::Receiver<UserSearchUpdate>,
) -> Vec<UserSearchUpdate> {
    collect_search_updates_with_timeout(rx, 30).await
}

/// Like [`collect_search_updates`] but with a custom timeout in seconds.
///
/// Use this for tests that involve network fetches across large follow graphs
/// where the default 30s may not be sufficient.
pub async fn collect_search_updates_with_timeout(
    mut rx: tokio::sync::broadcast::Receiver<UserSearchUpdate>,
    timeout_secs: u64,
) -> Vec<UserSearchUpdate> {
    let mut updates = Vec::new();
    let timeout = tokio::time::Duration::from_secs(timeout_secs);

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

/// Collects updates until a specific pubkey appears in a `ResultsFound` event.
///
/// Returns as soon as the target is found, without waiting for the full pipeline
/// to drain. This is much faster for searches that expand large follow graphs
/// where `SearchCompleted` can take tens of seconds after the match is already found.
///
/// Times out after 30 seconds.
pub async fn wait_for_result(
    mut rx: tokio::sync::broadcast::Receiver<UserSearchUpdate>,
    target: &PublicKey,
) -> Vec<UserSearchUpdate> {
    let mut updates = Vec::new();
    let timeout = tokio::time::Duration::from_secs(30);

    let result = tokio::time::timeout(timeout, async {
        loop {
            match rx.recv().await {
                Ok(update) => {
                    let found_target = matches!(update.trigger, SearchUpdateTrigger::ResultsFound)
                        && update.new_results.iter().any(|r| r.pubkey == *target);
                    let is_terminal = matches!(
                        update.trigger,
                        SearchUpdateTrigger::SearchCompleted { .. }
                            | SearchUpdateTrigger::Error { .. }
                    );
                    updates.push(update);
                    if found_target || is_terminal {
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
        tracing::warn!("wait_for_result timed out after {:?}", timeout);
    }

    updates
}

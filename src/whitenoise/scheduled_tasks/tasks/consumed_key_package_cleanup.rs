//! Scheduled task to clean up local MLS key material for consumed key packages.

use std::collections::HashSet;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, StreamExt};

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::database::published_key_packages::PublishedKeyPackage;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::scheduled_tasks::Task;
use crate::whitenoise::session::AccountSession;

/// Quiet period before cleaning up consumed key package local key material.
/// After this many seconds with no new welcomes for an account, it's safe
/// to delete local key material for consumed key packages.
const CONSUMED_KP_QUIET_PERIOD_SECS: i64 = 30;

/// Maximum number of accounts to process concurrently.
const MAX_CONCURRENT_ACCOUNTS: usize = 5;

/// Scheduled task that periodically cleans up local MLS key material for
/// consumed key packages after a quiet period.
///
/// When a Welcome message consumes a key package, the local key material is
/// not deleted immediately — multiple Welcomes may arrive in a burst. This
/// task waits for a quiet period with no new Welcomes before cleaning up,
/// ensuring all pending Welcomes can be processed first.
pub(crate) struct ConsumedKeyPackageCleanup;

#[async_trait]
impl Task for ConsumedKeyPackageCleanup {
    fn name(&self) -> &'static str {
        "consumed_key_package_cleanup"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(60 * 10) // 10 minutes
    }

    #[perf_instrument("scheduled::consumed_kp_cleanup")]
    async fn execute(&self, whitenoise: std::sync::Arc<Whitenoise>) -> Result<(), WhitenoiseError> {
        tracing::debug!(
            target: "whitenoise::scheduler::consumed_key_package_cleanup",
            "Starting consumed key package cleanup"
        );

        let accounts = Account::all(&whitenoise.shared.database).await?;

        if accounts.is_empty() {
            tracing::debug!(
                target: "whitenoise::scheduler::consumed_key_package_cleanup",
                "No accounts found, skipping"
            );
            return Ok(());
        }

        let cleanup_results: Vec<(String, Result<usize, WhitenoiseError>)> = stream::iter(accounts)
            .map(|account| {
                let whitenoise = whitenoise.clone();
                async move {
                    let pubkey_hex = account.pubkey.to_hex();
                    let result = cleanup_consumed_key_packages(&whitenoise, &account).await;
                    (pubkey_hex, result)
                }
            })
            .buffer_unordered(MAX_CONCURRENT_ACCOUNTS)
            .collect()
            .await;

        let total_cleaned = summarize_cleanup_results(cleanup_results);

        if total_cleaned > 0 {
            tracing::info!(
                target: "whitenoise::scheduler::consumed_key_package_cleanup",
                "Consumed key package cleanup: {} total cleaned",
                total_cleaned
            );
        }

        Ok(())
    }
}

/// Tallies cleanup results, logging per-account outcomes.
fn summarize_cleanup_results(results: Vec<(String, Result<usize, WhitenoiseError>)>) -> usize {
    let mut total_cleaned = 0usize;
    for (pubkey_hex, result) in results {
        match result {
            Ok(0) => {}
            Ok(cleaned) => {
                total_cleaned += cleaned;
                tracing::info!(
                    target: "whitenoise::scheduler::consumed_key_package_cleanup",
                    "Cleaned up {} consumed key package(s) for account {}",
                    cleaned,
                    pubkey_hex
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::scheduler::consumed_key_package_cleanup",
                    "Failed to clean up consumed key packages for account {}: {}",
                    pubkey_hex,
                    e
                );
            }
        }
    }
    total_cleaned
}

/// Cleans up local MLS key material for consumed key packages after the quiet period.
///
/// Checks if the account has consumed key packages where the quiet period has elapsed
/// (no new welcomes in the last 30 seconds), then deletes local key material using
/// the hash_ref stored at publish time and marks the row as cleaned.
#[perf_instrument("scheduled::consumed_kp_cleanup")]
async fn cleanup_consumed_key_packages(
    whitenoise: &Whitenoise,
    account: &Account,
) -> Result<usize, WhitenoiseError> {
    // Skip dormant accounts: cleanup reads/writes the per-account DB via
    // the session, so an account with no active session has nothing to
    // clean up this tick. Returning Ok(0) instead of erroring keeps the
    // scheduler quiet.
    let Some(session) = whitenoise.session(&account.pubkey) else {
        tracing::debug!(
            target: "whitenoise::scheduler::consumed_kp_cleanup",
            "Account {} has no active session, skipping",
            account.pubkey.to_hex()
        );
        return Ok(0);
    };
    let eligible = session
        .repos
        .published_key_packages
        .find_eligible_for_cleanup(CONSUMED_KP_QUIET_PERIOD_SECS)
        .await?;

    if eligible.is_empty() {
        return Ok(0);
    }

    tracing::debug!(
        target: "whitenoise::scheduler::consumed_key_package_cleanup",
        "Found {} consumed key package(s) eligible for cleanup for account {}",
        eligible.len(),
        account.pubkey.to_hex()
    );

    let mut cleaned_hash_refs = HashSet::new();
    let mut cleaned = 0usize;

    for consumed in &eligible {
        if !cleaned_hash_refs.insert(consumed.key_package_hash_ref.clone()) {
            continue;
        }

        match cleanup_consumed_key_package_material(&session, consumed).await {
            Ok(()) => {
                cleaned += 1;
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::scheduler::consumed_key_package_cleanup",
                    "Failed to delete local key material for consumed key package {}: {}",
                    consumed.id,
                    e
                );
            }
        }
    }

    Ok(cleaned)
}

async fn cleanup_consumed_key_package_material(
    session: &std::sync::Arc<AccountSession>,
    consumed: &PublishedKeyPackage,
) -> Result<(), WhitenoiseError> {
    session
        .key_packages()
        .delete_tracked_key_material(consumed)
        .await?;
    session
        .repos
        .published_key_packages
        .mark_key_material_deleted_by_hash_ref(&consumed.key_package_hash_ref)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use nostr_sdk::{EventBuilder, Keys, RelayUrl};

    use super::*;
    use crate::marmot::key_packages::key_package_from_base64_content;
    use crate::whitenoise::database::published_key_packages::PublishedKeyPackageProtocolData;
    use crate::whitenoise::key_packages::MLS_KEY_PACKAGE_KIND;
    use crate::whitenoise::session::test_helpers::test_session_with_marmot_keys;
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    #[test]
    fn test_task_properties() {
        let task = ConsumedKeyPackageCleanup;

        assert_eq!(task.name(), "consumed_key_package_cleanup");
        assert_eq!(task.interval(), Duration::from_secs(60 * 10));
    }

    #[tokio::test]
    async fn test_execute_with_no_accounts() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let task = ConsumedKeyPackageCleanup;
        let result = task.execute(whitenoise.clone()).await;

        assert!(result.is_ok());
    }

    #[test]
    fn test_summarize_cleanup_results_empty() {
        assert_eq!(summarize_cleanup_results(vec![]), 0);
    }

    #[test]
    fn test_summarize_cleanup_results_mixed() {
        let results = vec![
            ("a".to_string(), Ok(0)),
            ("b".to_string(), Ok(3)),
            ("c".to_string(), Ok(2)),
            ("d".to_string(), Err(WhitenoiseError::AccountNotFound)),
        ];
        assert_eq!(summarize_cleanup_results(results), 5);
    }

    #[test]
    fn test_summarize_cleanup_results_all_errors() {
        let results = vec![
            ("a".to_string(), Err(WhitenoiseError::AccountNotFound)),
            ("b".to_string(), Err(WhitenoiseError::AccountNotFound)),
        ];
        assert_eq!(summarize_cleanup_results(results), 0);
    }

    #[test]
    fn test_constants() {
        assert_eq!(CONSUMED_KP_QUIET_PERIOD_SECS, 30);
        assert_eq!(MAX_CONCURRENT_ACCOUNTS, 5);
    }

    #[tokio::test]
    async fn cleanup_consumed_key_package_material_handles_darkmatter_v2_records() {
        let keys = Keys::generate();
        let session = test_session_with_marmot_keys(keys.clone()).await;
        let relay_url = RelayUrl::parse("wss://kp.example").unwrap();
        let key_package_data = {
            let mut marmot = session.marmot.as_ref().unwrap().lock().await;
            marmot
                .fresh_key_package_event("cleanup-slot".to_string(), &[relay_url])
                .await
                .unwrap()
        };
        let key_package = key_package_from_base64_content(&key_package_data.content).unwrap();
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND, &key_package_data.content)
            .tags(key_package_data.tags.clone())
            .sign_with_keys(&keys)
            .unwrap();

        session
            .repos
            .published_key_packages
            .create_with_protocol_data(
                &key_package_data.key_package_ref,
                &event.id.to_hex(),
                MLS_KEY_PACKAGE_KIND,
                Some(&key_package_data.d_tag),
                PublishedKeyPackageProtocolData::darkmatter_v2_last_resort(
                    key_package_data.key_package_ref.clone(),
                    key_package_data.content.clone(),
                    key_package_data.app_components.clone(),
                ),
            )
            .await
            .unwrap();

        {
            let marmot = session.marmot.as_ref().unwrap().lock().await;
            assert!(marmot.has_key_package_material(&key_package).unwrap());
        }
        let record = session
            .repos
            .published_key_packages
            .find_by_event_id(&event.id.to_hex())
            .await
            .unwrap()
            .unwrap();

        cleanup_consumed_key_package_material(&session, &record)
            .await
            .unwrap();

        {
            let marmot = session.marmot.as_ref().unwrap().lock().await;
            assert!(!marmot.has_key_package_material(&key_package).unwrap());
        }
        let record = session
            .repos
            .published_key_packages
            .find_by_event_id(&event.id.to_hex())
            .await
            .unwrap()
            .unwrap();
        assert!(record.key_material_deleted);
    }
}

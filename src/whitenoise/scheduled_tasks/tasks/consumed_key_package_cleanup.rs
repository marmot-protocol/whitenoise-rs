//! Scheduled task to clean up local MLS key material for consumed key packages.

use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, StreamExt};

use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::database::published_key_packages::PublishedKeyPackage;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::scheduled_tasks::Task;

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

    async fn execute(&self, whitenoise: &'static Whitenoise) -> Result<(), WhitenoiseError> {
        tracing::debug!(
            target: "whitenoise::scheduler::consumed_key_package_cleanup",
            "Starting consumed key package cleanup"
        );

        let accounts = Account::all(&whitenoise.database).await?;

        if accounts.is_empty() {
            tracing::debug!(
                target: "whitenoise::scheduler::consumed_key_package_cleanup",
                "No accounts found, skipping"
            );
            return Ok(());
        }

        let cleanup_results: Vec<(String, Result<usize, WhitenoiseError>)> = stream::iter(accounts)
            .map(|account| async move {
                let pubkey_hex = account.pubkey.to_hex();
                let result = cleanup_consumed_key_packages(whitenoise, &account).await;
                (pubkey_hex, result)
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
async fn cleanup_consumed_key_packages(
    whitenoise: &Whitenoise,
    account: &Account,
) -> Result<usize, WhitenoiseError> {
    let eligible = PublishedKeyPackage::find_eligible_for_cleanup(
        &account.pubkey,
        CONSUMED_KP_QUIET_PERIOD_SECS,
        &whitenoise.database,
    )
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

    let mdk = whitenoise.create_mdk_for_account(account.pubkey)?;
    let mut cleaned = 0usize;

    for consumed in &eligible {
        match mdk.delete_key_package_from_storage_by_hash_ref(&consumed.key_package_hash_ref) {
            Ok(()) => {
                if let Err(e) = PublishedKeyPackage::mark_key_material_deleted(
                    consumed.id,
                    &whitenoise.database,
                )
                .await
                {
                    tracing::warn!(
                        target: "whitenoise::scheduler::consumed_key_package_cleanup",
                        "Deleted key material but failed to mark record {}: {}",
                        consumed.id,
                        e
                    );
                }
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

#[cfg(test)]
mod tests {
    use super::*;
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
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));

        let task = ConsumedKeyPackageCleanup;
        let result = task.execute(whitenoise).await;

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
}

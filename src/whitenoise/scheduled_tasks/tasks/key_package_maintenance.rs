use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use nostr_sdk::{Event, Timestamp};

use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::database::consumed_key_packages::ConsumedKeyPackage;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::scheduled_tasks::Task;

/// Maximum age for a key package before it should be rotated (30 days).
const KEY_PACKAGE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Quiet period before cleaning up consumed key package local key material.
/// After this many seconds with no new welcomes for an account, it's safe
/// to delete local key material for consumed key packages.
const CONSUMED_KP_QUIET_PERIOD_SECS: i64 = 30;

/// Maximum number of accounts to process concurrently.
const MAX_CONCURRENT_ACCOUNTS: usize = 5;

pub(crate) struct KeyPackageMaintenance;

#[async_trait]
impl Task for KeyPackageMaintenance {
    fn name(&self) -> &'static str {
        "key_package_maintenance"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(60 * 10)
    }

    async fn execute(&self, whitenoise: &'static Whitenoise) -> Result<(), WhitenoiseError> {
        tracing::debug!(
            target: "whitenoise::scheduled_tasks::key_package_maintenance",
            "Starting key package maintenance"
        );

        let accounts = Account::all(&whitenoise.database).await?;

        if accounts.is_empty() {
            tracing::debug!(
                target: "whitenoise::scheduled_tasks::key_package_maintenance",
                "No accounts found, skipping"
            );
            return Ok(());
        }

        let results: Vec<MaintenanceResult> = stream::iter(accounts.clone())
            .map(|account| async move { maintain_key_packages(whitenoise, &account).await })
            .buffer_unordered(MAX_CONCURRENT_ACCOUNTS)
            .collect()
            .await;

        let summary = summarize_maintenance_results(results);

        tracing::info!(
            target: "whitenoise::scheduled_tasks::key_package_maintenance",
            "Key package maintenance completed: {} checked, {} published, {} rotated, {} skipped, {} errors",
            summary.checked,
            summary.published,
            summary.rotated,
            summary.skipped,
            summary.errors
        );

        // Phase 2: Clean up local key material for consumed key packages
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
                target: "whitenoise::scheduled_tasks::key_package_maintenance",
                "Consumed key package cleanup: {} total cleaned",
                total_cleaned
            );
        }

        Ok(())
    }
}

enum MaintenanceResult {
    /// Key package is fresh, no action needed
    Fresh,
    /// Published a new key package (account had none)
    Published,
    /// Rotated: published new and deleted old key package(s)
    Rotated { deleted: usize },
    /// Account has no key package relays configured
    Skipped,
    /// An error occurred
    Error(WhitenoiseError),
}

/// Summary counters from Phase 1 maintenance results.
#[derive(Debug, Default, PartialEq, Eq)]
struct MaintenanceSummary {
    checked: usize,
    published: usize,
    rotated: usize,
    skipped: usize,
    errors: usize,
}

/// Tallies Phase 1 maintenance results into summary counters.
fn summarize_maintenance_results(results: Vec<MaintenanceResult>) -> MaintenanceSummary {
    let mut summary = MaintenanceSummary::default();
    for result in results {
        summary.checked += 1;
        match result {
            MaintenanceResult::Fresh => {}
            MaintenanceResult::Published => summary.published += 1,
            MaintenanceResult::Rotated { deleted } => {
                summary.rotated += 1;
                tracing::debug!(
                    target: "whitenoise::scheduled_tasks::key_package_maintenance",
                    "Rotated key package, deleted {} old one(s)",
                    deleted
                );
            }
            MaintenanceResult::Skipped => summary.skipped += 1,
            MaintenanceResult::Error(e) => {
                summary.errors += 1;
                tracing::warn!(
                    target: "whitenoise::scheduled_tasks::key_package_maintenance",
                    "Error during key package maintenance: {}",
                    e
                );
            }
        }
    }
    summary
}

/// Tallies Phase 2 cleanup results, logging per-account outcomes.
fn summarize_cleanup_results(results: Vec<(String, Result<usize, WhitenoiseError>)>) -> usize {
    let mut total_cleaned = 0usize;
    for (pubkey_hex, result) in results {
        match result {
            Ok(0) => {}
            Ok(cleaned) => {
                total_cleaned += cleaned;
                tracing::info!(
                    target: "whitenoise::scheduled_tasks::key_package_maintenance",
                    "Cleaned up {} consumed key package(s) for account {}",
                    cleaned,
                    pubkey_hex
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::scheduled_tasks::key_package_maintenance",
                    "Failed to clean up consumed key packages for account {}: {}",
                    pubkey_hex,
                    e
                );
            }
        }
    }
    total_cleaned
}

async fn maintain_key_packages(whitenoise: &Whitenoise, account: &Account) -> MaintenanceResult {
    let packages = match whitenoise.fetch_all_key_packages_for_account(account).await {
        Ok(packages) => packages,
        Err(WhitenoiseError::AccountMissingKeyPackageRelays) => {
            tracing::debug!(
                target: "whitenoise::scheduled_tasks::key_package_maintenance",
                "Account {} has no key package relays configured, skipping",
                account.pubkey.to_hex()
            );
            return MaintenanceResult::Skipped;
        }
        Err(e) => return MaintenanceResult::Error(e),
    };

    // Case 1: No key packages - publish a new one
    if packages.is_empty() {
        return publish_new_key_package(whitenoise, account).await;
    }

    // Case 2: Check for expired packages
    let expired_packages = find_expired_packages(&packages);

    if expired_packages.is_empty() {
        tracing::debug!(
            target: "whitenoise::scheduled_tasks::key_package_maintenance",
            "Account {} has {} fresh key package(s)",
            account.pubkey.to_hex(),
            packages.len()
        );
        return MaintenanceResult::Fresh;
    }

    // Publish new key package first, then delete expired ones
    rotate_expired_packages(whitenoise, account, expired_packages).await
}

/// Returns key packages that are older than the maximum age threshold.
fn find_expired_packages(packages: &[Event]) -> Vec<Event> {
    let now = Timestamp::now();
    let max_age_secs = KEY_PACKAGE_MAX_AGE.as_secs();

    packages
        .iter()
        .filter(|p| now.as_secs().saturating_sub(p.created_at.as_secs()) >= max_age_secs)
        .cloned()
        .collect()
}

/// Publishes a new key package when account has none.
async fn publish_new_key_package(whitenoise: &Whitenoise, account: &Account) -> MaintenanceResult {
    tracing::info!(
        target: "whitenoise::scheduled_tasks::key_package_maintenance",
        "Account {} has no key packages, publishing new one",
        account.pubkey.to_hex()
    );

    match whitenoise.publish_key_package_for_account(account).await {
        Ok(()) => {
            tracing::info!(
                target: "whitenoise::scheduled_tasks::key_package_maintenance",
                "Published key package for account {}",
                account.pubkey.to_hex()
            );
            MaintenanceResult::Published
        }
        Err(WhitenoiseError::AccountMissingKeyPackageRelays) => MaintenanceResult::Skipped,
        Err(e) => MaintenanceResult::Error(e),
    }
}

/// Publishes a new key package, then deletes expired ones.
async fn rotate_expired_packages(
    whitenoise: &Whitenoise,
    account: &Account,
    expired_packages: Vec<Event>,
) -> MaintenanceResult {
    tracing::info!(
        target: "whitenoise::scheduled_tasks::key_package_maintenance",
        "Account {} has {} expired key package(s), rotating",
        account.pubkey.to_hex(),
        expired_packages.len()
    );

    // Publish new key package first (so there's no gap)
    if let Err(e) = whitenoise.publish_key_package_for_account(account).await {
        match e {
            WhitenoiseError::AccountMissingKeyPackageRelays => return MaintenanceResult::Skipped,
            _ => return MaintenanceResult::Error(e),
        }
    }

    tracing::debug!(
        target: "whitenoise::scheduled_tasks::key_package_maintenance",
        "Published new key package for account {}, now deleting {} expired one(s)",
        account.pubkey.to_hex(),
        expired_packages.len()
    );

    // Delete expired key packages (don't delete MLS stored keys for now)
    match whitenoise
        .delete_key_packages_for_account(account, expired_packages, false, 1)
        .await
    {
        Ok(deleted) => MaintenanceResult::Rotated { deleted },
        Err(e) => {
            // Log but don't fail - we successfully published a new one
            tracing::warn!(
                target: "whitenoise::scheduled_tasks::key_package_maintenance",
                "Failed to delete expired key packages for account {}: {}",
                account.pubkey.to_hex(),
                e
            );
            MaintenanceResult::Rotated { deleted: 0 }
        }
    }
}

/// Cleans up local MLS key material for consumed key packages after the quiet period.
///
/// Checks if the account has consumed key packages where the quiet period has elapsed
/// (no new welcomes in the last 30 seconds), then deletes local key material using
/// the cached serialized hash_ref.
async fn cleanup_consumed_key_packages(
    whitenoise: &Whitenoise,
    account: &Account,
) -> Result<usize, WhitenoiseError> {
    let eligible = ConsumedKeyPackage::find_eligible_for_cleanup(
        &account.pubkey,
        CONSUMED_KP_QUIET_PERIOD_SECS,
        &whitenoise.database,
    )
    .await?;

    if eligible.is_empty() {
        return Ok(0);
    }

    tracing::debug!(
        target: "whitenoise::scheduled_tasks::key_package_maintenance",
        "Found {} consumed key package(s) eligible for cleanup for account {}",
        eligible.len(),
        account.pubkey.to_hex()
    );

    let mdk = whitenoise.create_mdk_for_account(account.pubkey)?;
    let mut cleaned = 0usize;

    for consumed in &eligible {
        match mdk.delete_key_package_from_storage_by_hash_ref(&consumed.key_package_hash_ref) {
            Ok(()) => {
                if let Err(e) = ConsumedKeyPackage::delete(consumed.id, &whitenoise.database).await
                {
                    tracing::warn!(
                        target: "whitenoise::scheduled_tasks::key_package_maintenance",
                        "Deleted key material but failed to remove tracking record {}: {}",
                        consumed.id,
                        e
                    );
                }
                cleaned += 1;
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::scheduled_tasks::key_package_maintenance",
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
        let task = KeyPackageMaintenance;

        assert_eq!(task.name(), "key_package_maintenance");
        assert_eq!(task.interval(), Duration::from_secs(60 * 10)); // 10 minutes
    }

    #[tokio::test]
    async fn test_execute_with_no_accounts() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));

        let task = KeyPackageMaintenance;
        let result = task.execute(whitenoise).await;

        // Should succeed - just logs "No accounts found, skipping"
        assert!(result.is_ok());
    }

    #[test]
    fn test_find_expired_packages_returns_old_packages() {
        let keys = nostr_sdk::Keys::generate();

        // Create an event with a timestamp 31 days in the past
        let old_timestamp = nostr_sdk::Timestamp::now() - Duration::from_secs(31 * 24 * 60 * 60);
        let old_event = nostr_sdk::EventBuilder::new(nostr_sdk::Kind::MlsKeyPackage, "old")
            .custom_created_at(old_timestamp)
            .sign_with_keys(&keys)
            .unwrap();

        // Create an event with a fresh timestamp
        let fresh_event = nostr_sdk::EventBuilder::new(nostr_sdk::Kind::MlsKeyPackage, "fresh")
            .sign_with_keys(&keys)
            .unwrap();

        let packages = vec![old_event.clone(), fresh_event.clone()];
        let expired = find_expired_packages(&packages);

        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, old_event.id);
    }

    #[test]
    fn test_find_expired_packages_returns_empty_when_all_fresh() {
        let keys = nostr_sdk::Keys::generate();

        let fresh1 = nostr_sdk::EventBuilder::new(nostr_sdk::Kind::MlsKeyPackage, "fresh1")
            .sign_with_keys(&keys)
            .unwrap();
        let fresh2 = nostr_sdk::EventBuilder::new(nostr_sdk::Kind::MlsKeyPackage, "fresh2")
            .sign_with_keys(&keys)
            .unwrap();

        let expired = find_expired_packages(&[fresh1, fresh2]);
        assert!(expired.is_empty());
    }

    #[test]
    fn test_find_expired_packages_handles_empty_input() {
        let expired = find_expired_packages(&[]);
        assert!(expired.is_empty());
    }

    #[test]
    fn test_summarize_maintenance_results_empty() {
        let summary = summarize_maintenance_results(vec![]);
        assert_eq!(summary, MaintenanceSummary::default());
    }

    #[test]
    fn test_summarize_maintenance_results_mixed() {
        let results = vec![
            MaintenanceResult::Fresh,
            MaintenanceResult::Published,
            MaintenanceResult::Rotated { deleted: 3 },
            MaintenanceResult::Skipped,
            MaintenanceResult::Error(WhitenoiseError::AccountNotFound),
            MaintenanceResult::Fresh,
        ];

        let summary = summarize_maintenance_results(results);
        assert_eq!(summary.checked, 6);
        assert_eq!(summary.published, 1);
        assert_eq!(summary.rotated, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.errors, 1);
    }

    #[test]
    fn test_summarize_maintenance_results_all_fresh() {
        let results = vec![
            MaintenanceResult::Fresh,
            MaintenanceResult::Fresh,
            MaintenanceResult::Fresh,
        ];
        let summary = summarize_maintenance_results(results);
        assert_eq!(summary.checked, 3);
        assert_eq!(summary.published, 0);
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
        assert_eq!(KEY_PACKAGE_MAX_AGE, Duration::from_secs(30 * 24 * 60 * 60));
        assert_eq!(CONSUMED_KP_QUIET_PERIOD_SECS, 30);
        assert_eq!(MAX_CONCURRENT_ACCOUNTS, 5);
    }

    // NOTE: Relay-dependent tests (publish when none exist, leave fresh
    // packages alone, rotate expired packages) live in the integration test
    // suite at src/integration_tests/test_cases/scheduler/key_package_maintenance.rs
    // and are exercised via `just int-test scheduler`.
}

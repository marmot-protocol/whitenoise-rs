use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use nostr_sdk::{Event, Timestamp};

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::database::published_key_packages::PublishedKeyPackage;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::scheduled_tasks::Task;

/// Maximum age for a key package before it should be rotated (30 days).
const KEY_PACKAGE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

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

    #[perf_instrument("scheduled::key_package_maintenance")]
    async fn execute(&self, whitenoise: &'static Whitenoise) -> Result<(), WhitenoiseError> {
        tracing::debug!(
            target: "whitenoise::scheduler::key_package_maintenance",
            "Starting key package maintenance"
        );

        let accounts = Account::all(&whitenoise.database).await?;

        if accounts.is_empty() {
            tracing::debug!(
                target: "whitenoise::scheduler::key_package_maintenance",
                "No accounts found, skipping"
            );
            return Ok(());
        }

        let results: Vec<MaintenanceResult> = stream::iter(accounts)
            .map(|account| async move { maintain_key_packages(whitenoise, &account).await })
            .buffer_unordered(MAX_CONCURRENT_ACCOUNTS)
            .collect()
            .await;

        let summary = summarize_maintenance_results(results);

        tracing::info!(
            target: "whitenoise::scheduler::key_package_maintenance",
            "Key package maintenance completed: {} checked, {} published, \
             {} rotated (expired), {} skipped, {} errors",
            summary.checked,
            summary.published,
            summary.rotated_expired,
            summary.skipped,
            summary.errors
        );

        Ok(())
    }
}

enum MaintenanceResult {
    /// Key package is fresh, no action needed
    Fresh,
    /// Published a new key package because the account had no usable local one
    Published,
    /// Rotated expired key packages (>30 days old)
    RotatedExpired { deleted: usize },
    /// Account has no key package relays configured
    Skipped,
    /// An error occurred
    Error(WhitenoiseError),
}

/// Summary counters from maintenance results.
#[derive(Debug, PartialEq, Eq, Default)]
struct MaintenanceSummary {
    checked: usize,
    published: usize,
    rotated_expired: usize,
    skipped: usize,
    errors: usize,
}

/// Tallies maintenance results into summary counters.
fn summarize_maintenance_results(results: Vec<MaintenanceResult>) -> MaintenanceSummary {
    let mut summary = MaintenanceSummary::default();
    for result in results {
        summary.checked += 1;
        match result {
            MaintenanceResult::Fresh => {}
            MaintenanceResult::Published => summary.published += 1,
            MaintenanceResult::RotatedExpired { deleted } => {
                summary.rotated_expired += 1;
                tracing::debug!(
                    target: "whitenoise::scheduler::key_package_maintenance",
                    "Rotated expired key package, deleted {} old one(s)",
                    deleted
                );
            }
            MaintenanceResult::Skipped => summary.skipped += 1,
            MaintenanceResult::Error(e) => {
                summary.errors += 1;
                tracing::warn!(
                    target: "whitenoise::scheduler::key_package_maintenance",
                    "Error during key package maintenance: {}",
                    e
                );
            }
        }
    }
    summary
}

#[perf_instrument("scheduled::key_package_maintenance")]
async fn maintain_key_packages(whitenoise: &Whitenoise, account: &Account) -> MaintenanceResult {
    let packages = match whitenoise.fetch_all_key_packages_for_account(account).await {
        Ok(packages) => packages,
        Err(WhitenoiseError::AccountMissingKeyPackageRelays) => {
            tracing::debug!(
                target: "whitenoise::scheduler::key_package_maintenance",
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

    // Case 2: Check whether any relay key packages still belong to this app
    // and still have live local key material. This mirrors the login-time
    // recovery path: if every relay package is foreign or already consumed on
    // this device, publish a fresh one.
    let our_packages = match find_live_published_key_packages(whitenoise, account, packages).await {
        Ok(packages) => packages,
        Err(e) => return MaintenanceResult::Error(e),
    };

    if our_packages.is_empty() {
        tracing::info!(
            target: "whitenoise::scheduler::key_package_maintenance",
            "Account {} has key package events on relays but none with live local state, publishing new one",
            account.pubkey.to_hex()
        );
        return publish_new_key_package(whitenoise, account).await;
    }

    // Case 3: Check for expired packages (>30 days old)
    let our_expired_packages = find_expired_packages(&our_packages);

    if our_expired_packages.is_empty() {
        tracing::debug!(
            target: "whitenoise::scheduler::key_package_maintenance",
            "Account {} has {} live tracked key package(s), none expired",
            account.pubkey.to_hex(),
            our_packages.len()
        );
        return MaintenanceResult::Fresh;
    }

    // Delete expired packages, only republishing if all our packages are
    // expired (so we'd have zero left after deletion).
    rotate_expired_packages(
        whitenoise,
        account,
        our_expired_packages,
        our_packages.len(),
    )
    .await
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

/// Returns relay key packages that this device published and still has usable local state for.
#[perf_instrument("scheduled::key_package_maintenance")]
async fn find_live_published_key_packages(
    whitenoise: &Whitenoise,
    account: &Account,
    packages: Vec<Event>,
) -> Result<Vec<Event>, WhitenoiseError> {
    let mut our_packages = Vec::new();

    for event in packages {
        match PublishedKeyPackage::find_by_event_id(
            &account.pubkey,
            &event.id.to_hex(),
            &whitenoise.database,
        )
        .await?
        {
            Some(pkg) if !pkg.key_material_deleted && pkg.consumed_at.is_none() => {
                our_packages.push(event)
            }
            Some(_) | None => {}
        }
    }

    Ok(our_packages)
}

/// Publishes a new key package when the account has no usable local one.
#[perf_instrument("scheduled::key_package_maintenance")]
async fn publish_new_key_package(whitenoise: &Whitenoise, account: &Account) -> MaintenanceResult {
    tracing::info!(
        target: "whitenoise::scheduler::key_package_maintenance",
        "Account {} has no usable local key package state, publishing new one",
        account.pubkey.to_hex()
    );

    match whitenoise.publish_key_package_for_account(account).await {
        Ok(()) => {
            tracing::info!(
                target: "whitenoise::scheduler::key_package_maintenance",
                "Published key package for account {}",
                account.pubkey.to_hex()
            );
            MaintenanceResult::Published
        }
        Err(WhitenoiseError::AccountMissingKeyPackageRelays) => MaintenanceResult::Skipped,
        Err(e) => MaintenanceResult::Error(e),
    }
}

/// Deletes expired key packages, only publishing a replacement if needed.
///
/// If the account would be left with zero key packages after deletion, a new one is
/// published first to avoid a gap. Otherwise, only the expired packages are deleted
/// without republishing, since the account already has a valid package.
#[perf_instrument("scheduled::key_package_maintenance")]
async fn rotate_expired_packages(
    whitenoise: &Whitenoise,
    account: &Account,
    expired_packages: Vec<Event>,
    total_package_count: usize,
) -> MaintenanceResult {
    let non_expired_count = total_package_count - expired_packages.len();

    tracing::info!(
        target: "whitenoise::scheduler::key_package_maintenance",
        "Account {} has {} expired key package(s) and {} non-expired, cleaning up",
        account.pubkey.to_hex(),
        expired_packages.len(),
        non_expired_count
    );

    // Only publish a new key package if deleting the expired ones would leave zero packages
    if non_expired_count == 0 {
        if let Err(e) = whitenoise.publish_key_package_for_account(account).await {
            match e {
                WhitenoiseError::AccountMissingKeyPackageRelays => {
                    return MaintenanceResult::Skipped;
                }
                _ => return MaintenanceResult::Error(e),
            }
        }

        tracing::debug!(
            target: "whitenoise::scheduler::key_package_maintenance",
            "Published new key package for account {} (all existing were expired)",
            account.pubkey.to_hex(),
        );
    }

    // Delete expired key packages (don't delete MLS stored keys for now)
    match whitenoise
        .delete_key_packages_for_account(account, expired_packages, false, 1)
        .await
    {
        Ok(deleted) => MaintenanceResult::RotatedExpired { deleted },
        Err(e) => {
            tracing::warn!(
                target: "whitenoise::scheduler::key_package_maintenance",
                "Failed to delete expired key packages for account {}: {}",
                account.pubkey.to_hex(),
                e
            );
            MaintenanceResult::RotatedExpired { deleted: 0 }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::database::published_key_packages::PublishedKeyPackage;
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    #[tokio::test]
    async fn test_execute_republishes_when_local_key_material_is_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));

        let account = whitenoise.create_identity().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let before = whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await
            .unwrap();
        assert_eq!(
            before.len(),
            2,
            "Should start with the canonical and legacy key package twins"
        );

        let tracked = PublishedKeyPackage::find_by_event_id(
            &account.pubkey,
            &before[0].id.to_hex(),
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();

        let mdk = whitenoise.create_mdk_for_account(account.pubkey).unwrap();
        mdk.delete_key_package_from_storage_by_hash_ref(&tracked.key_package_hash_ref)
            .unwrap();
        PublishedKeyPackage::mark_key_material_deleted_by_hash_ref(
            &account.pubkey,
            &tracked.key_package_hash_ref,
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Validate the precondition for this regression: the relay event still
        // exists, but this device no longer has any live local state for it.
        let live_before = find_live_published_key_packages(whitenoise, &account, before)
            .await
            .unwrap();
        assert!(
            live_before.is_empty(),
            "No relay key package should remain tracked as live after deleting local key material"
        );

        let task = KeyPackageMaintenance;
        task.execute(whitenoise).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let after = whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await
            .unwrap();
        let live_after = find_live_published_key_packages(whitenoise, &account, after)
            .await
            .unwrap();
        assert!(
            !live_after.is_empty(),
            "Maintenance should publish a fresh key package when relay packages no longer match live local state"
        );
    }

    #[tokio::test]
    async fn test_execute_republishes_when_only_consumed_key_packages_remain() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));

        let account = whitenoise.create_identity().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let before = whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await
            .unwrap();
        assert_eq!(
            before.len(),
            2,
            "Should start with the canonical and legacy key package twins"
        );

        PublishedKeyPackage::mark_consumed(
            &account.pubkey,
            &before[0].id.to_hex(),
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Validate the precondition for this regression: the relay event still
        // exists, but the tracked package has already been consumed locally.
        let live_before = find_live_published_key_packages(whitenoise, &account, before)
            .await
            .unwrap();
        assert!(
            live_before.is_empty(),
            "Consumed key packages should not count as live local state"
        );

        let task = KeyPackageMaintenance;
        task.execute(whitenoise).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let after = whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await
            .unwrap();
        let live_after = find_live_published_key_packages(whitenoise, &account, after)
            .await
            .unwrap();
        assert!(
            !live_after.is_empty(),
            "Maintenance should publish a fresh key package when only consumed packages remain"
        );
    }

    // NOTE: Other relay-dependent tests (publish when none exist, leave fresh
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
            MaintenanceResult::RotatedExpired { deleted: 3 },
            MaintenanceResult::Skipped,
            MaintenanceResult::Error(WhitenoiseError::AccountNotFound),
            MaintenanceResult::Fresh,
        ];

        let summary = summarize_maintenance_results(results);
        assert_eq!(summary.checked, 6);
        assert_eq!(summary.published, 1);
        assert_eq!(summary.rotated_expired, 1);
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
    fn test_constants() {
        assert_eq!(KEY_PACKAGE_MAX_AGE, Duration::from_secs(30 * 24 * 60 * 60));
        assert_eq!(MAX_CONCURRENT_ACCOUNTS, 5);
    }

    // NOTE: Relay-dependent tests (publish when none exist, leave fresh
    // packages alone, rotate expired packages) live in the integration test
    // suite at src/integration_tests/test_cases/scheduler/key_package_maintenance.rs
    // and are exercised via `just int-test scheduler`.
}

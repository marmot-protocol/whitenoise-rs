use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use nostr_sdk::{Event, Timestamp};

use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::key_packages::find_outdated_packages;
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

        let mut checked = 0usize;
        let mut published = 0usize;
        let mut rotated_expired = 0usize;
        let mut deleted_outdated = 0usize;
        let mut skipped = 0usize;
        let mut errors = 0usize;

        let results: Vec<MaintenanceResult> = stream::iter(accounts)
            .map(|account| async move { maintain_key_packages(whitenoise, &account).await })
            .buffer_unordered(MAX_CONCURRENT_ACCOUNTS)
            .collect()
            .await;

        // Summarize results
        for result in results {
            checked += 1;
            match result {
                MaintenanceResult::Fresh => {}
                MaintenanceResult::Published => {
                    published += 1;
                }
                MaintenanceResult::RotatedExpired { deleted } => {
                    rotated_expired += 1;
                    tracing::debug!(
                        target: "whitenoise::scheduler::key_package_maintenance",
                        "Rotated expired key package, deleted {} old one(s)",
                        deleted
                    );
                }
                MaintenanceResult::DeletedOutdated { deleted } => {
                    deleted_outdated += 1;
                    tracing::info!(
                        target: "whitenoise::scheduler::key_package_maintenance",
                        "Deleted {} outdated key package(s) missing encoding tag",
                        deleted
                    );
                }
                MaintenanceResult::Skipped => {
                    skipped += 1;
                }
                MaintenanceResult::Error(e) => {
                    errors += 1;
                    tracing::warn!(
                        target: "whitenoise::scheduler::key_package_maintenance",
                        "Error during key package maintenance: {}",
                        e
                    );
                }
            }
        }

        tracing::info!(
            target: "whitenoise::scheduler::key_package_maintenance",
            "Key package maintenance completed: {} checked, {} published, {} rotated (expired), {} deleted (outdated), {} skipped, {} errors",
            checked,
            published,
            rotated_expired,
            deleted_outdated,
            skipped,
            errors
        );

        Ok(())
    }
}

enum MaintenanceResult {
    /// Key package is fresh, no action needed
    Fresh,
    /// Published a new key package (account had none)
    Published,
    /// Rotated expired key packages (>30 days old)
    RotatedExpired { deleted: usize },
    /// Deleted outdated key packages (missing encoding tag) without republishing
    DeletedOutdated { deleted: usize },
    /// Account has no key package relays configured
    Skipped,
    /// An error occurred
    Error(WhitenoiseError),
}

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

    // Case 2: Check for outdated packages (missing encoding tag)
    // These are broken for newer MDK versions and should be cleaned up.
    // We only delete outdated packages if there are also valid (non-outdated) packages
    // remaining for this account, to avoid leaving the account with zero packages.
    // We do NOT republish a new key package here -- ideally each device maintains a single
    // key package that only gets rotated when used.
    let outdated_packages = find_outdated_packages(&packages);

    if !outdated_packages.is_empty() {
        let valid_package_count = packages.len() - outdated_packages.len();

        if valid_package_count > 0 {
            tracing::info!(
                target: "whitenoise::scheduler::key_package_maintenance",
                "Account {} has {} outdated key package(s) missing encoding tag and {} valid package(s), deleting outdated",
                account.pubkey.to_hex(),
                outdated_packages.len(),
                valid_package_count
            );
            return delete_outdated_packages(whitenoise, account, outdated_packages).await;
        } else {
            // All packages are outdated -- publish a new valid one instead of deleting.
            // The outdated packages will naturally age out and be caught by the expired
            // package rotation (30 days), or will be cleaned up once a valid package exists.
            tracing::info!(
                target: "whitenoise::scheduler::key_package_maintenance",
                "Account {} has {} outdated key package(s) but no valid packages, publishing new one",
                account.pubkey.to_hex(),
                outdated_packages.len()
            );
            return publish_new_key_package(whitenoise, account).await;
        }
    }

    // Case 3: Check for expired packages (>30 days old)
    // Only target expired packages that are parseable by our local MDK instance.
    // This filters out packages published by other Marmot apps that we can't verify ownership of.
    let expired_packages = find_expired_packages(&packages);
    let our_expired_packages =
        whitenoise.filter_locally_parseable_key_packages(account, expired_packages);

    if our_expired_packages.is_empty() {
        tracing::debug!(
            target: "whitenoise::scheduler::key_package_maintenance",
            "Account {} has {} fresh key package(s)",
            account.pubkey.to_hex(),
            packages.len()
        );
        return MaintenanceResult::Fresh;
    }

    // Delete expired packages, only publishing a replacement if the account would have none left
    rotate_expired_packages(whitenoise, account, our_expired_packages, packages.len()).await
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
        target: "whitenoise::scheduler::key_package_maintenance",
        "Account {} has no key packages, publishing new one",
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

/// Deletes outdated key packages (missing encoding tag) without publishing a replacement.
///
/// Outdated key packages were published before the MIP-00/MIP-02 encoding tag requirement
/// was enforced. They cause interop failures with newer MDK versions. We only delete
/// them (without republishing) because the account already has at least one valid key
/// package. This avoids unnecessary key package churn.
async fn delete_outdated_packages(
    whitenoise: &Whitenoise,
    account: &Account,
    outdated_packages: Vec<Event>,
) -> MaintenanceResult {
    match whitenoise
        .delete_key_packages_for_account(account, outdated_packages, false, 1)
        .await
    {
        Ok(deleted) => MaintenanceResult::DeletedOutdated { deleted },
        Err(e) => {
            tracing::warn!(
                target: "whitenoise::scheduler::key_package_maintenance",
                "Failed to delete outdated key packages for account {}: {}",
                account.pubkey.to_hex(),
                e
            );
            MaintenanceResult::DeletedOutdated { deleted: 0 }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::key_packages::has_encoding_tag;
    use crate::whitenoise::relays::Relay;
    use crate::whitenoise::test_utils::create_mock_whitenoise;
    use nostr_sdk::{
        Client, EventBuilder, EventId, FromBech32, Keys, Kind, SecretKey, Tag, TagKind,
    };

    /// Publishes a key package with a backdated timestamp for testing rotation.
    async fn publish_backdated_key_package(
        whitenoise: &Whitenoise,
        account: &crate::whitenoise::accounts::Account,
        relays: &[Relay],
        days_old: u64,
    ) -> Result<EventId, crate::whitenoise::error::WhitenoiseError> {
        let (encoded_key_package, tags) = whitenoise.encoded_key_package(account, relays).await?;

        let nsec = whitenoise.export_account_nsec(account).await?;
        let secret_key =
            SecretKey::from_bech32(&nsec).map_err(|e| WhitenoiseError::Other(e.into()))?;
        let keys = Keys::new(secret_key);

        let backdated = Timestamp::now() - Duration::from_secs(days_old * 24 * 60 * 60);

        let event = EventBuilder::new(Kind::MlsKeyPackage, &encoded_key_package)
            .tags(tags.to_vec())
            .custom_created_at(backdated)
            .sign_with_keys(&keys)
            .map_err(|e| WhitenoiseError::Other(e.into()))?;

        let event_id = event.id;

        let client = Client::default();
        for relay in relays {
            client.add_relay(relay.url.as_str()).await?;
        }
        client.connect().await;
        client.set_signer(keys).await;
        client.send_event(&event).await?;
        client.disconnect().await;

        Ok(event_id)
    }

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

    #[tokio::test]
    async fn test_execute_publishes_key_package_when_none_exist() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));

        // Create account (automatically sets up key package relays)
        let account = whitenoise.create_identity().await.unwrap();

        // Delete any existing key packages to start clean
        whitenoise
            .delete_all_key_packages_for_account(&account, true)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify no packages exist
        let before = whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await
            .unwrap();
        assert!(before.is_empty(), "Should start with no key packages");

        // Run maintenance
        let task = KeyPackageMaintenance;
        task.execute(whitenoise).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Should have published a key package
        let after = whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await
            .unwrap();
        assert!(
            !after.is_empty(),
            "Maintenance should have published a key package"
        );
    }

    #[tokio::test]
    async fn test_execute_leaves_fresh_packages_alone() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));

        // Create account (this automatically publishes a key package)
        let account = whitenoise.create_identity().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let before = whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await
            .unwrap();
        assert!(!before.is_empty(), "Account should have a key package");
        let original_ids: std::collections::HashSet<_> = before.iter().map(|e| e.id).collect();

        // Run maintenance
        let task = KeyPackageMaintenance;
        task.execute(whitenoise).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Should still have the same key packages (not rotated since they're fresh)
        let after = whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await
            .unwrap();
        let after_ids: std::collections::HashSet<_> = after.iter().map(|e| e.id).collect();

        assert_eq!(
            original_ids, after_ids,
            "Fresh key packages should not be rotated"
        );
    }

    #[tokio::test]
    async fn test_execute_rotates_expired_packages() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));

        // Create account and get its key package relays
        let account = whitenoise.create_identity().await.unwrap();
        let kp_relays = account.key_package_relays(whitenoise).await.unwrap();

        // Delete any existing key packages
        whitenoise
            .delete_all_key_packages_for_account(&account, true)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Publish a backdated key package (31 days old, exceeds 30-day threshold)
        let expired_event_id = publish_backdated_key_package(whitenoise, &account, &kp_relays, 31)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify we have the expired package
        let before = whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await
            .unwrap();
        assert_eq!(before.len(), 1, "Should have exactly one key package");
        assert_eq!(
            before[0].id, expired_event_id,
            "Should be our expired package"
        );

        // Run maintenance - should rotate the expired package
        let task = KeyPackageMaintenance;
        task.execute(whitenoise).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify rotation occurred
        let after = whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await
            .unwrap();

        // Should have at least one package (the new one)
        assert!(
            !after.is_empty(),
            "Should have a key package after rotation"
        );

        // The expired package should be gone
        let expired_still_exists = after.iter().any(|e| e.id == expired_event_id);
        assert!(
            !expired_still_exists,
            "Expired key package should have been deleted"
        );
    }

    /// Publishes a key package without the encoding tag for testing outdated package rotation.
    async fn publish_outdated_key_package(
        whitenoise: &Whitenoise,
        account: &crate::whitenoise::accounts::Account,
        relays: &[Relay],
    ) -> Result<EventId, crate::whitenoise::error::WhitenoiseError> {
        let (encoded_key_package, tags) = whitenoise.encoded_key_package(account, relays).await?;

        let nsec = whitenoise.export_account_nsec(account).await?;
        let secret_key =
            SecretKey::from_bech32(&nsec).map_err(|e| WhitenoiseError::Other(e.into()))?;
        let keys = Keys::new(secret_key);

        // Filter out the encoding tag to simulate an outdated key package
        let tags_without_encoding: Vec<Tag> = tags
            .into_iter()
            .filter(|tag| tag.kind() != TagKind::Custom("encoding".into()))
            .collect();

        let event = EventBuilder::new(Kind::MlsKeyPackage, &encoded_key_package)
            .tags(tags_without_encoding)
            .sign_with_keys(&keys)
            .map_err(|e| WhitenoiseError::Other(e.into()))?;

        let event_id = event.id;

        let client = Client::default();
        for relay in relays {
            client.add_relay(relay.url.as_str()).await?;
        }
        client.connect().await;
        client.set_signer(keys).await;
        client.send_event(&event).await?;
        client.disconnect().await;

        Ok(event_id)
    }

    #[tokio::test]
    async fn test_execute_deletes_outdated_packages_when_valid_exists() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));

        // Create account (this publishes a valid key package automatically)
        let account = whitenoise.create_identity().await.unwrap();
        let kp_relays = account.key_package_relays(whitenoise).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Also publish an outdated key package (missing encoding tag)
        let outdated_event_id = publish_outdated_key_package(whitenoise, &account, &kp_relays)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify we have both packages
        let before = whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await
            .unwrap();
        assert_eq!(before.len(), 2, "Should have two key packages");

        let has_outdated = before.iter().any(|e| e.id == outdated_event_id);
        assert!(has_outdated, "Should have the outdated package");

        let valid_count = before.iter().filter(|e| has_encoding_tag(e)).count();
        assert_eq!(valid_count, 1, "Should have one valid package");

        // Run maintenance - should delete only the outdated package (no republish)
        let task = KeyPackageMaintenance;
        task.execute(whitenoise).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify only the outdated package was deleted
        let after = whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await
            .unwrap();

        // Should still have the valid package
        assert_eq!(after.len(), 1, "Should have exactly one key package");
        assert!(
            has_encoding_tag(&after[0]),
            "Remaining package should have encoding tag"
        );

        // The outdated package should be gone
        let outdated_still_exists = after.iter().any(|e| e.id == outdated_event_id);
        assert!(
            !outdated_still_exists,
            "Outdated key package should have been deleted"
        );
    }

    #[tokio::test]
    async fn test_execute_publishes_new_when_all_packages_outdated() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));

        // Create account and get its key package relays
        let account = whitenoise.create_identity().await.unwrap();
        let kp_relays = account.key_package_relays(whitenoise).await.unwrap();

        // Delete any existing key packages
        whitenoise
            .delete_all_key_packages_for_account(&account, true)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Publish only an outdated key package (missing encoding tag)
        publish_outdated_key_package(whitenoise, &account, &kp_relays)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify we have only the outdated package
        let before = whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await
            .unwrap();
        assert_eq!(before.len(), 1, "Should have exactly one key package");
        assert!(
            !has_encoding_tag(&before[0]),
            "Package should be outdated (no encoding tag)"
        );

        // Run maintenance - should publish a new valid package (not delete the outdated one)
        let task = KeyPackageMaintenance;
        task.execute(whitenoise).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify a new valid package was published
        let after = whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await
            .unwrap();

        // Should have at least the new valid package
        let valid_packages: Vec<_> = after.iter().filter(|e| has_encoding_tag(e)).collect();
        assert!(
            !valid_packages.is_empty(),
            "Should have a new valid key package"
        );

        // The outdated package may still exist (we didn't delete it)
        // It will age out via the expired package rotation
    }

    #[test]
    fn test_find_expired_packages_identifies_old_packages() {
        let keys = Keys::generate();

        // Create a fresh event (just created)
        let fresh = EventBuilder::new(Kind::MlsKeyPackage, "fresh")
            .sign_with_keys(&keys)
            .unwrap();

        // Create an expired event (31 days old)
        let expired_timestamp = Timestamp::now() - Duration::from_secs(31 * 24 * 60 * 60);
        let expired = EventBuilder::new(Kind::MlsKeyPackage, "expired")
            .custom_created_at(expired_timestamp)
            .sign_with_keys(&keys)
            .unwrap();

        let packages = vec![fresh.clone(), expired.clone()];
        let result = find_expired_packages(&packages);

        assert_eq!(result.len(), 1, "Should find exactly one expired package");
        assert_eq!(
            result[0].id, expired.id,
            "Expired package should be the old one"
        );
    }

    #[test]
    fn test_find_expired_packages_returns_empty_when_all_fresh() {
        let keys = Keys::generate();

        let fresh1 = EventBuilder::new(Kind::MlsKeyPackage, "fresh1")
            .sign_with_keys(&keys)
            .unwrap();
        let fresh2 = EventBuilder::new(Kind::MlsKeyPackage, "fresh2")
            .sign_with_keys(&keys)
            .unwrap();

        let packages = vec![fresh1, fresh2];
        let result = find_expired_packages(&packages);

        assert!(
            result.is_empty(),
            "Should return empty when all packages are fresh"
        );
    }

    #[test]
    fn test_find_expired_packages_handles_empty_list() {
        let packages: Vec<Event> = vec![];
        let result = find_expired_packages(&packages);

        assert!(result.is_empty(), "Should return empty for empty input");
    }

    #[test]
    fn test_find_expired_packages_boundary_not_expired_at_29_days() {
        let keys = Keys::generate();

        // 29 days old - should NOT be expired (threshold is 30 days)
        let almost_expired_timestamp = Timestamp::now() - Duration::from_secs(29 * 24 * 60 * 60);
        let almost_expired = EventBuilder::new(Kind::MlsKeyPackage, "almost")
            .custom_created_at(almost_expired_timestamp)
            .sign_with_keys(&keys)
            .unwrap();

        let packages = vec![almost_expired];
        let result = find_expired_packages(&packages);

        assert!(
            result.is_empty(),
            "Package at 29 days should not be considered expired"
        );
    }

    #[test]
    fn test_find_expired_packages_boundary_expired_at_30_days() {
        let keys = Keys::generate();

        // Exactly 30 days old - should be expired (>= threshold)
        let expired_timestamp = Timestamp::now() - Duration::from_secs(30 * 24 * 60 * 60);
        let at_threshold = EventBuilder::new(Kind::MlsKeyPackage, "threshold")
            .custom_created_at(expired_timestamp)
            .sign_with_keys(&keys)
            .unwrap();

        let packages = vec![at_threshold.clone()];
        let result = find_expired_packages(&packages);

        assert_eq!(
            result.len(),
            1,
            "Package at exactly 30 days should be considered expired"
        );
        assert_eq!(result[0].id, at_threshold.id);
    }
}

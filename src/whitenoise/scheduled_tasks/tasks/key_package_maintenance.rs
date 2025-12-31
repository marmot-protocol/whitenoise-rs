use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use nostr_sdk::{Event, Timestamp};

use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
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
        let mut rotated = 0usize;
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
                MaintenanceResult::Rotated { deleted } => {
                    rotated += 1;
                    tracing::debug!(
                        target: "whitenoise::scheduler::key_package_maintenance",
                        "Rotated key package, deleted {} old one(s)",
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
            "Key package maintenance completed: {} checked, {} published, {} rotated, {} skipped, {} errors",
            checked,
            published,
            rotated,
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
    /// Rotated: published new and deleted old key package(s)
    Rotated { deleted: usize },
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

    // Case 2: Check for expired packages
    let expired_packages = find_expired_packages(&packages);

    if expired_packages.is_empty() {
        tracing::debug!(
            target: "whitenoise::scheduler::key_package_maintenance",
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
        .filter(|p| now.as_u64().saturating_sub(p.created_at.as_u64()) >= max_age_secs)
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

/// Publishes a new key package, then deletes expired ones.
async fn rotate_expired_packages(
    whitenoise: &Whitenoise,
    account: &Account,
    expired_packages: Vec<Event>,
) -> MaintenanceResult {
    tracing::info!(
        target: "whitenoise::scheduler::key_package_maintenance",
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
        target: "whitenoise::scheduler::key_package_maintenance",
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
                target: "whitenoise::scheduler::key_package_maintenance",
                "Failed to delete expired key packages for account {}: {}",
                account.pubkey.to_hex(),
                e
            );
            MaintenanceResult::Rotated { deleted: 0 }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::relays::Relay;
    use crate::whitenoise::test_utils::create_mock_whitenoise;
    use nostr_sdk::{Client, EventBuilder, EventId, FromBech32, Keys, Kind, SecretKey};

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
}

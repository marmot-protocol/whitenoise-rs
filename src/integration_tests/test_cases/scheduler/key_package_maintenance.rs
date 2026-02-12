use std::time::Duration;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::whitenoise::relays::Relay;
use crate::whitenoise::scheduled_tasks::{KeyPackageMaintenance, Task};
use async_trait::async_trait;
use nostr_sdk::prelude::*;

/// Verifies the key package maintenance task handles both cases:
/// 1. Publishes key packages when none exist
/// 2. Rotates expired key packages (deletes old, publishes new)
pub struct KeyPackageMaintenanceTestCase {
    account_name: String,
}

impl KeyPackageMaintenanceTestCase {
    pub fn for_account(name: &str) -> Self {
        Self {
            account_name: name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for KeyPackageMaintenanceTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Testing key package maintenance for account: {}",
            self.account_name
        );

        // Create an account (this sets up key package relays automatically)
        let account = context.whitenoise.create_identity().await?;
        tracing::info!("✓ Created account: {}", account.pubkey.to_hex());
        context.add_account(&self.account_name, account.clone());

        // Verify account has key package relays configured
        let kp_relays = account.key_package_relays(context.whitenoise).await?;
        assert!(
            !kp_relays.is_empty(),
            "Account should have key package relays configured"
        );
        tracing::info!(
            "✓ Account has {} key package relay(s) configured",
            kp_relays.len()
        );

        // Delete any existing key packages to start with a clean slate
        let deleted = context
            .whitenoise
            .delete_all_key_packages_for_account(&account, true)
            .await?;
        tracing::info!("✓ Deleted {} existing key package(s)", deleted);

        // Wait for relays to process deletions
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        let before_count = context
            .whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await?
            .len();
        assert_eq!(before_count, 0, "Should have 0 key packages after deletion");
        tracing::info!("✓ Verified 0 key packages exist");

        // Run maintenance
        let task = KeyPackageMaintenance;
        task.execute(context.whitenoise).await?;

        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        let after_publish = context
            .whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await?;
        assert!(
            !after_publish.is_empty(),
            "Should have at least one key package after maintenance"
        );
        tracing::info!(
            "✓ Key package maintenance published {} key package(s)",
            after_publish.len()
        );

        // Delete current key packages and publish an expired one
        context
            .whitenoise
            .delete_all_key_packages_for_account(&account, true)
            .await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // Publish a backdated key package (31 days old)
        let expired_event_id =
            publish_backdated_key_package(context, &account, &kp_relays, 31).await?;
        tracing::info!(
            "✓ Published expired key package: {}",
            expired_event_id.to_hex()
        );

        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // Verify we have exactly 1 expired key package
        let before_rotate = context
            .whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await?;
        assert_eq!(
            before_rotate.len(),
            1,
            "Should have exactly 1 key package before rotation"
        );
        assert_eq!(
            before_rotate[0].id, expired_event_id,
            "The key package should be our expired one"
        );
        tracing::info!("✓ Verified 1 expired key package exists");

        // Run maintenance - should publish new and delete expired
        task.execute(context.whitenoise).await?;

        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // Verify rotation: old one deleted, new one published
        let after_rotate = context
            .whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await?;

        // Should have at least 1 key package (the new one)
        assert!(
            !after_rotate.is_empty(),
            "Should have at least one key package after rotation"
        );

        // The expired one should be gone
        let expired_still_exists = after_rotate.iter().any(|e| e.id == expired_event_id);
        assert!(
            !expired_still_exists,
            "Expired key package should have been deleted"
        );

        tracing::info!(
            "✓ Rotation complete: expired package deleted, {} fresh package(s) exist",
            after_rotate.len()
        );

        Ok(())
    }
}

/// Publishes a key package with a backdated timestamp using test infrastructure.
async fn publish_backdated_key_package(
    context: &ScenarioContext,
    account: &crate::Account,
    relays: &[Relay],
    days_old: u64,
) -> Result<EventId, WhitenoiseError> {
    // Get the encoded key package and tags
    let (encoded_key_package, tags, _hash_ref) = context
        .whitenoise
        .encoded_key_package(account, relays)
        .await?;

    // Get the account's secret key via public API
    let nsec = context.whitenoise.export_account_nsec(account).await?;
    let secret_key = SecretKey::from_bech32(&nsec).map_err(|e| WhitenoiseError::Other(e.into()))?;
    let keys = Keys::new(secret_key);

    // Calculate the backdated timestamp
    let backdated = Timestamp::now() - Duration::from_secs(days_old * 24 * 60 * 60);

    // Build and sign the event with custom timestamp
    let event = EventBuilder::new(Kind::MlsKeyPackage, &encoded_key_package)
        .tags(tags.to_vec())
        .custom_created_at(backdated)
        .sign_with_keys(&keys)
        .map_err(|e| WhitenoiseError::Other(e.into()))?;

    let event_id = event.id;

    // Create a test client and publish
    let relay_urls: Vec<&str> = relays.iter().map(|r| r.url.as_str()).collect();
    let client = create_test_client(&relay_urls, keys).await?;
    client.send_event(&event).await?;
    client.disconnect().await;

    tracing::debug!(
        "Published backdated key package {} ({}d old)",
        event_id.to_hex(),
        days_old
    );

    Ok(event_id)
}

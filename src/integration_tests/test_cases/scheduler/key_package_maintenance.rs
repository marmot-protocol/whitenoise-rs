use std::time::Duration;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::whitenoise::key_packages::MLS_KEY_PACKAGE_KIND_LEGACY;
use crate::whitenoise::relays::Relay;
use crate::whitenoise::scheduled_tasks::{KeyPackageMaintenance, Task};
use async_trait::async_trait;
use nostr_sdk::prelude::*;

/// Maximum number of fetch+delete rounds before giving up. Each round can
/// only clear what a single relay query returns (NIP-01 pagination), so a
/// modest bound is enough to handle realistic relay behaviour without
/// looping forever if a relay refuses to honour deletion requests.
const MAX_DELETE_ROUNDS: u32 = 10;

/// Deletes every key package event on the account's configured key-package
/// relays, looping until the relays return an empty set or
/// [`MAX_DELETE_ROUNDS`] is exhausted.
///
/// Private to this test: the scheduler maintenance test is the only consumer
/// that needs to clear an account's relay-published KPs. The legacy KP
/// fixture used to also use this helper, but that fixture now relies on
/// [`crate::Whitenoise::create_identity_without_initial_key_package`] so
/// nothing pre-exists to delete.
///
/// `delete_mls_stored_keys`, when `true`, is forwarded to the very first
/// deletion round so the locally cached MDK private key for any KP we
/// remove gets cleaned up too. Subsequent rounds always pass `false` —
/// the local key state for those KPs is already gone after round 0.
///
/// Returns the total number of key packages deleted across all rounds.
async fn delete_all_relay_key_packages_for_test_setup(
    context: &ScenarioContext,
    account: &crate::Account,
    delete_mls_stored_keys: bool,
) -> Result<usize, WhitenoiseError> {
    let mut total_deleted = 0;

    for round in 0..MAX_DELETE_ROUNDS {
        let key_packages = context
            .whitenoise
            .require_session(&account.pubkey)?
            .key_packages()
            .fetch_all()
            .await?;

        if key_packages.is_empty() {
            return Ok(total_deleted);
        }

        let key_package_count = key_packages.len();
        let delete_mls_stored_keys_this_round = delete_mls_stored_keys && round == 0;
        let deleted = context
            .whitenoise
            .require_session(&account.pubkey)?
            .key_packages()
            .delete_batch(key_packages, delete_mls_stored_keys_this_round, 1)
            .await?;

        total_deleted += deleted;

        if deleted == 0 {
            tracing::warn!(
                target: "whitenoise::integration_tests::key_package_cleanup",
                "Deleted 0 key package(s) despite {} remaining after {} relay cleanup round(s)",
                key_package_count,
                round + 1,
            );
            break;
        }
    }

    // Cap-exhaustion / stalled-deletion path: re-check rather than silently
    // claiming success. A test fixture that proceeds with a polluted relay
    // pushes the real failure to a downstream wait or assert and turns a
    // clear setup error into a flake.
    let remaining = context
        .whitenoise
        .require_session(&account.pubkey)?
        .key_packages()
        .fetch_all()
        .await?;
    if !remaining.is_empty() {
        return Err(WhitenoiseError::Internal(format!(
            "key-package cleanup exhausted {MAX_DELETE_ROUNDS} round(s) with \
             {} event(s) still present on relays",
            remaining.len()
        )));
    }

    Ok(total_deleted)
}

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
        let kp_relays = account
            .key_package_relays(&context.whitenoise.shared)
            .await?;
        assert!(
            !kp_relays.is_empty(),
            "Account should have key package relays configured"
        );
        tracing::info!(
            "✓ Account has {} key package relay(s) configured",
            kp_relays.len()
        );

        // `create_identity()` publishes the initial key package in a background
        // task when running against the real singleton used by integration tests.
        // Wait for that publish to land before trying to clean the relays.
        let initially_published = wait_for_key_packages(
            context,
            &account,
            "initial key package publication after account creation",
            |packages| !packages.is_empty(),
        )
        .await?;
        tracing::info!(
            "✓ Initial account setup published {} key package(s)",
            initially_published.len()
        );

        // Delete any existing key packages to start with a clean slate. This
        // test needs a fully empty relay state, while the developer-facing
        // legacy cleanup intentionally removes only kind:443 copies.
        let deleted = delete_all_relay_key_packages_for_test_setup(context, &account, true).await?;
        tracing::info!("✓ Deleted {} existing key package(s)", deleted);

        let before_delete = wait_for_key_packages(
            context,
            &account,
            "key package relays to be empty after deletion",
            |packages| packages.is_empty(),
        )
        .await?;
        assert_eq!(
            before_delete.len(),
            0,
            "Should have 0 key packages after deletion"
        );
        tracing::info!("✓ Verified 0 key packages exist");

        // Run maintenance
        let task = KeyPackageMaintenance;
        task.execute(context.whitenoise.clone()).await?;

        let after_publish = wait_for_key_packages(
            context,
            &account,
            "maintenance to publish a replacement key package",
            |packages| !packages.is_empty(),
        )
        .await?;
        tracing::info!(
            "✓ Key package maintenance published {} key package(s)",
            after_publish.len()
        );

        // Delete current key packages and publish an expired one.
        delete_all_relay_key_packages_for_test_setup(context, &account, true).await?;
        wait_for_key_packages(
            context,
            &account,
            "key package relays to be empty before publishing an expired package",
            |packages| packages.is_empty(),
        )
        .await?;

        // Publish a backdated key package (31 days old)
        let expired_event_id =
            publish_backdated_key_package(context, &account, &kp_relays, 31).await?;
        tracing::info!(
            "✓ Published expired key package: {}",
            expired_event_id.to_hex()
        );

        // Verify we have exactly 1 expired key package
        let before_rotate = wait_for_key_packages(
            context,
            &account,
            "the expired key package to be the only package before rotation",
            |packages| packages.len() == 1 && packages[0].id == expired_event_id,
        )
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
        task.execute(context.whitenoise.clone()).await?;

        // Verify rotation: old one deleted, new one published
        let after_rotate = wait_for_key_packages(
            context,
            &account,
            "expired key package rotation to complete",
            |packages| !packages.is_empty() && packages.iter().all(|e| e.id != expired_event_id),
        )
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

async fn wait_for_key_packages<F>(
    context: &ScenarioContext,
    account: &crate::Account,
    description: &str,
    predicate: F,
) -> Result<Vec<Event>, WhitenoiseError>
where
    F: Fn(&[Event]) -> bool + Copy,
{
    let whitenoise = &context.whitenoise;
    let account = account.clone();

    retry(
        50,
        Duration::from_millis(100),
        || {
            let account = account.clone();
            async move {
                let key_packages = whitenoise
                    .require_session(&account.pubkey)?
                    .key_packages()
                    .fetch_all()
                    .await?;

                if predicate(&key_packages) {
                    Ok(key_packages)
                } else {
                    Err(WhitenoiseError::Internal(format!(
                        "Observed {} key package(s) while waiting for {}",
                        key_packages.len(),
                        description,
                    )))
                }
            }
        },
        description,
    )
    .await
}

/// Publishes a key package with a backdated timestamp using test infrastructure.
async fn publish_backdated_key_package(
    context: &ScenarioContext,
    account: &crate::Account,
    relays: &[Relay],
    days_old: u64,
) -> Result<EventId, WhitenoiseError> {
    // Get the encoded key package and tags
    let key_package_data = context
        .whitenoise
        .encoded_key_package(account, relays)
        .await?;

    // Get the account's secret key via public API
    let nsec = context.whitenoise.export_account_nsec(account).await?;
    let secret_key =
        SecretKey::from_bech32(&nsec).map_err(|e| WhitenoiseError::Internal(e.to_string()))?;
    let keys = Keys::new(secret_key);

    // Calculate the backdated timestamp
    let backdated = Timestamp::now() - Duration::from_secs(days_old * 24 * 60 * 60);

    // Build and sign the event with custom timestamp
    let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, &key_package_data.content)
        .tags(key_package_data.tags_443.to_vec())
        .custom_created_at(backdated)
        .sign_with_keys(&keys)
        .map_err(|e| WhitenoiseError::Internal(e.to_string()))?;

    let event_id = event.id;

    // Create a test client and publish
    let relay_urls: Vec<&str> = relays.iter().map(|r| r.url.as_str()).collect();
    let client = create_test_client(&relay_urls, keys).await?;
    client.send_event(&event).await?;
    client.disconnect().await;

    context
        .whitenoise
        .require_session(&account.pubkey)?
        .key_packages()
        .track_published_for_testing(&key_package_data.hash_ref, &event_id.to_hex())
        .await?;

    tracing::debug!(
        "Published backdated key package {} ({}d old)",
        event_id.to_hex(),
        days_old
    );

    Ok(event_id)
}

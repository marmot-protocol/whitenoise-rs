//! Scheduled task to verify and re-publish missing relay list events.
//!
//! When login makes inbox (kind 10050) and key-package (kind 10051) relay list
//! publishes non-fatal (warn-and-continue after NIP-65 succeeds), a transient
//! network failure can leave the account in a state where the local DB has the
//! correct relay lists but the network is missing the events.
//!
//! This task periodically checks each logged-in account and re-publishes any
//! relay lists that exist locally but are absent from the network.

use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, StreamExt};

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::relays::{Relay, RelayType};
use crate::whitenoise::scheduled_tasks::Task;

/// Maximum number of accounts to process concurrently.
const MAX_CONCURRENT_ACCOUNTS: usize = 5;

/// Relay list types to check and potentially re-publish.
/// NIP-65 is excluded because its publish failure is already fatal during login.
const RELAY_TYPES_TO_CHECK: [RelayType; 2] = [RelayType::Inbox, RelayType::KeyPackage];

pub(crate) struct RelayListMaintenance;

#[async_trait]
impl Task for RelayListMaintenance {
    fn name(&self) -> &'static str {
        "relay_list_maintenance"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(60 * 30) // 30 minutes
    }

    #[perf_instrument("scheduled::relay_list_maintenance")]
    async fn execute(&self, whitenoise: &'static Whitenoise) -> Result<(), WhitenoiseError> {
        tracing::debug!(
            target: "whitenoise::scheduler::relay_list_maintenance",
            "Starting relay list maintenance"
        );

        let accounts = Account::all(&whitenoise.database).await?;

        if accounts.is_empty() {
            tracing::debug!(
                target: "whitenoise::scheduler::relay_list_maintenance",
                "No accounts found, skipping"
            );
            return Ok(());
        }

        let results: Vec<MaintenanceResult> = stream::iter(accounts)
            .map(|account| async move { maintain_relay_lists(whitenoise, &account).await })
            .buffer_unordered(MAX_CONCURRENT_ACCOUNTS)
            .flat_map(stream::iter)
            .collect()
            .await;

        let summary = summarize_maintenance_results(results);

        tracing::info!(
            target: "whitenoise::scheduler::relay_list_maintenance",
            "Relay list maintenance completed: {} checked, {} published, \
             {} already present, {} skipped (no local relays), {} errors",
            summary.checked,
            summary.published,
            summary.already_present,
            summary.skipped,
            summary.errors
        );

        Ok(())
    }
}

enum MaintenanceResult {
    /// Relay list exists on the network, no action needed.
    AlreadyPresent,
    /// Re-published a missing relay list to the network.
    Published,
    /// Account has no local relays configured for this type.
    Skipped,
    /// An error occurred during check or publish.
    Error(WhitenoiseError),
}

#[derive(Debug, PartialEq, Eq, Default)]
struct MaintenanceSummary {
    checked: usize,
    published: usize,
    already_present: usize,
    skipped: usize,
    errors: usize,
}

fn summarize_maintenance_results(results: Vec<MaintenanceResult>) -> MaintenanceSummary {
    let mut summary = MaintenanceSummary::default();
    for result in results {
        summary.checked += 1;
        match result {
            MaintenanceResult::AlreadyPresent => summary.already_present += 1,
            MaintenanceResult::Published => summary.published += 1,
            MaintenanceResult::Skipped => summary.skipped += 1,
            MaintenanceResult::Error(e) => {
                summary.errors += 1;
                tracing::warn!(
                    target: "whitenoise::scheduler::relay_list_maintenance",
                    "Error during relay list maintenance: {}",
                    e
                );
            }
        }
    }
    summary
}

/// Checks all relay list types for a single account and re-publishes any that
/// are missing from the network.
#[perf_instrument("scheduled::relay_list_maintenance")]
async fn maintain_relay_lists(
    whitenoise: &Whitenoise,
    account: &Account,
) -> Vec<MaintenanceResult> {
    let mut results = Vec::with_capacity(RELAY_TYPES_TO_CHECK.len());

    for relay_type in &RELAY_TYPES_TO_CHECK {
        results.push(check_and_republish(whitenoise, account, *relay_type).await);
    }

    results
}

/// Checks whether a specific relay list type exists on the network for the
/// given account, and re-publishes it if missing.
#[perf_instrument("scheduled::relay_list_maintenance")]
async fn check_and_republish(
    whitenoise: &Whitenoise,
    account: &Account,
    relay_type: RelayType,
) -> MaintenanceResult {
    // 1. Read local relay config from DB
    let local_relays = match account.relays(relay_type, whitenoise).await {
        Ok(relays) => relays,
        Err(e) => return MaintenanceResult::Error(e),
    };

    if local_relays.is_empty() {
        tracing::debug!(
            target: "whitenoise::scheduler::relay_list_maintenance",
            "Account {} has no local {:?} relays, skipping",
            account.pubkey.to_hex(),
            relay_type
        );
        return MaintenanceResult::Skipped;
    }

    // 2. Determine where to look for the event on the network.
    //    Use NIP-65 relays as the source, falling back to the relay list's own
    //    relays when NIP-65 is empty (e.g. the account only has inbox/kp relays).
    let source_relays = match account.nip65_relays(whitenoise).await {
        Ok(nip65) if !nip65.is_empty() => nip65,
        Ok(_) => local_relays.clone(),
        Err(e) => {
            tracing::warn!(
                target: "whitenoise::scheduler::relay_list_maintenance",
                "Failed to fetch NIP-65 relays for account {}, \
                 falling back to {:?} relays: {}",
                account.pubkey.to_hex(),
                relay_type,
                e
            );
            local_relays.clone()
        }
    };
    let source_urls = Relay::urls(&source_relays);

    // 3. Query the network for the existing relay list event
    let network_event = match whitenoise
        .relay_control
        .fetch_user_relays(account.pubkey, relay_type, &source_urls)
        .await
    {
        Ok(event) => event,
        Err(e) => {
            tracing::warn!(
                target: "whitenoise::scheduler::relay_list_maintenance",
                "Failed to fetch {:?} relay list from network for account {}: {}",
                relay_type,
                account.pubkey.to_hex(),
                e
            );
            return MaintenanceResult::Error(WhitenoiseError::NostrManager(e));
        }
    };

    // 4. If the relay list exists on the network, nothing to do
    if network_event.is_some() {
        tracing::debug!(
            target: "whitenoise::scheduler::relay_list_maintenance",
            "Account {} {:?} relay list is present on network",
            account.pubkey.to_hex(),
            relay_type
        );
        return MaintenanceResult::AlreadyPresent;
    }

    // 5. Missing from network — re-publish
    tracing::info!(
        target: "whitenoise::scheduler::relay_list_maintenance",
        "Account {} {:?} relay list is missing from network, re-publishing",
        account.pubkey.to_hex(),
        relay_type
    );

    let signer = match whitenoise.get_signer_for_account(account) {
        Ok(signer) => signer,
        Err(e) => {
            tracing::warn!(
                target: "whitenoise::scheduler::relay_list_maintenance",
                "Cannot get signer for account {} to re-publish {:?} relay list: {}",
                account.pubkey.to_hex(),
                relay_type,
                e
            );
            return MaintenanceResult::Error(e);
        }
    };

    let relay_urls = Relay::urls(&local_relays);
    let target_urls = Relay::urls(&source_relays);

    match whitenoise
        .relay_control
        .publish_relay_list_with_signer(&relay_urls, relay_type, &target_urls, signer)
        .await
    {
        Ok(()) => {
            tracing::info!(
                target: "whitenoise::scheduler::relay_list_maintenance",
                "Successfully re-published {:?} relay list for account {}",
                relay_type,
                account.pubkey.to_hex()
            );
            MaintenanceResult::Published
        }
        Err(e) => {
            tracing::warn!(
                target: "whitenoise::scheduler::relay_list_maintenance",
                "Failed to re-publish {:?} relay list for account {}: {}",
                relay_type,
                account.pubkey.to_hex(),
                e
            );
            MaintenanceResult::Error(WhitenoiseError::NostrManager(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    #[test]
    fn test_task_properties() {
        let task = RelayListMaintenance;

        assert_eq!(task.name(), "relay_list_maintenance");
        assert_eq!(task.interval(), Duration::from_secs(60 * 30)); // 30 minutes
    }

    #[tokio::test]
    async fn test_execute_with_no_accounts() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));

        let task = RelayListMaintenance;
        let result = task.execute(whitenoise).await;

        assert!(result.is_ok());
    }

    #[test]
    fn test_relay_types_to_check() {
        // Verify we check inbox and key-package but not NIP-65
        assert_eq!(RELAY_TYPES_TO_CHECK.len(), 2);
        assert!(RELAY_TYPES_TO_CHECK.contains(&RelayType::Inbox));
        assert!(RELAY_TYPES_TO_CHECK.contains(&RelayType::KeyPackage));
    }

    #[test]
    fn test_summarize_maintenance_results_empty() {
        let summary = summarize_maintenance_results(vec![]);
        assert_eq!(summary, MaintenanceSummary::default());
    }

    #[test]
    fn test_summarize_maintenance_results_mixed() {
        let results = vec![
            MaintenanceResult::AlreadyPresent,
            MaintenanceResult::Published,
            MaintenanceResult::Skipped,
            MaintenanceResult::Error(WhitenoiseError::AccountNotFound),
            MaintenanceResult::AlreadyPresent,
        ];

        let summary = summarize_maintenance_results(results);
        assert_eq!(summary.checked, 5);
        assert_eq!(summary.published, 1);
        assert_eq!(summary.already_present, 2);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.errors, 1);
    }

    #[test]
    fn test_summarize_maintenance_results_all_present() {
        let results = vec![
            MaintenanceResult::AlreadyPresent,
            MaintenanceResult::AlreadyPresent,
            MaintenanceResult::AlreadyPresent,
        ];
        let summary = summarize_maintenance_results(results);
        assert_eq!(summary.checked, 3);
        assert_eq!(summary.already_present, 3);
        assert_eq!(summary.published, 0);
    }

    #[test]
    fn test_summarize_maintenance_results_all_skipped() {
        let results = vec![MaintenanceResult::Skipped, MaintenanceResult::Skipped];
        let summary = summarize_maintenance_results(results);
        assert_eq!(summary.checked, 2);
        assert_eq!(summary.skipped, 2);
        assert_eq!(summary.published, 0);
    }

    #[tokio::test]
    async fn test_execute_is_noop_when_relay_lists_already_present() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));

        // Create account — this publishes relay lists to the local test relays
        let account = whitenoise.create_identity().await.unwrap();

        // Verify the account has local relay config
        let inbox_relays = account.inbox_relays(whitenoise).await.unwrap();
        assert!(!inbox_relays.is_empty(), "Account should have inbox relays");

        let kp_relays = account.key_package_relays(whitenoise).await.unwrap();
        assert!(
            !kp_relays.is_empty(),
            "Account should have key package relays"
        );

        // Run maintenance — relay lists should already be on the network
        // (published during create_identity), so this should be a no-op
        let task = RelayListMaintenance;
        let result = task.execute(whitenoise).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_maintain_relay_lists_skips_when_no_local_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let whitenoise: &'static Whitenoise = Box::leak(Box::new(whitenoise));

        // Create a bare account with no relay config — simulates an account
        // where relay list publishing failed and no local relays were saved.
        let keys = nostr_sdk::Keys::generate();
        let account = Account::new_external(whitenoise, keys.public_key())
            .await
            .unwrap();
        let account = account.save(&whitenoise.database).await.unwrap();

        // Verify no local relays exist
        let inbox = account.inbox_relays(whitenoise).await.unwrap();
        let kp = account.key_package_relays(whitenoise).await.unwrap();
        assert!(inbox.is_empty(), "Should have no inbox relays");
        assert!(kp.is_empty(), "Should have no key package relays");

        // Run maintain_relay_lists directly — both types should be Skipped
        let results = maintain_relay_lists(whitenoise, &account).await;
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|r| matches!(r, MaintenanceResult::Skipped)),
            "Both relay types should be skipped when no local relays exist"
        );
    }

    #[test]
    fn test_constants() {
        assert_eq!(MAX_CONCURRENT_ACCOUNTS, 5);
        assert_eq!(RELAY_TYPES_TO_CHECK.len(), 2);
    }
}

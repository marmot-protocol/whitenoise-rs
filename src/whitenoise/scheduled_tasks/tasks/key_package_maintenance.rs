use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use nostr_sdk::{Event, Timestamp};

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::key_packages::{
    MLS_KEY_PACKAGE_KIND, REQUIRED_MLS_CIPHERSUITE_TAG, validate_marmot_key_package_strict,
};
use crate::whitenoise::scheduled_tasks::Task;

/// Maximum age for a key package before it should be rotated (30 days).
const KEY_PACKAGE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Maximum number of accounts to process concurrently.
const MAX_CONCURRENT_ACCOUNTS: usize = 5;

#[derive(Debug, Clone)]
struct LivePublishedKeyPackage {
    event: Event,
    key_package_hash_ref: Vec<u8>,
}

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
    async fn execute(&self, whitenoise: std::sync::Arc<Whitenoise>) -> Result<(), WhitenoiseError> {
        tracing::debug!(
            target: "whitenoise::scheduler::key_package_maintenance",
            "Starting key package maintenance"
        );

        let accounts = Account::all(&whitenoise.shared.database).await?;

        if accounts.is_empty() {
            tracing::debug!(
                target: "whitenoise::scheduler::key_package_maintenance",
                "No accounts found, skipping"
            );
            return Ok(());
        }

        let results: Vec<MaintenanceResult> = stream::iter(accounts)
            .map(|account| {
                let whitenoise = whitenoise.clone();
                async move { maintain_key_packages(&whitenoise, &account).await }
            })
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
    // Skip dormant accounts: maintenance reads/writes the per-account DB via
    // the session, so an account with no active session has nothing for the
    // scheduler to do this tick.
    let Some(session) = whitenoise.session(&account.pubkey) else {
        tracing::debug!(
            target: "whitenoise::scheduler::key_package_maintenance",
            "Account {} has no active session, skipping",
            account.pubkey.to_hex()
        );
        return MaintenanceResult::Skipped;
    };

    let packages = match session.key_packages().fetch_all().await {
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
    let our_packages = filter_compatible_key_packages(our_packages);

    if our_packages.is_empty() {
        tracing::info!(
            target: "whitenoise::scheduler::key_package_maintenance",
            "Account {} has key package events on relays but none with live compatible local state, publishing new one",
            account.pubkey.to_hex()
        );
        return publish_new_key_package(whitenoise, account).await;
    }

    // Case 3: Check for expired packages (>30 days old)
    let our_expired_packages = find_expired_packages(&our_packages);
    if our_expired_packages.is_empty() {
        if !has_required_live_key_package_set(&our_packages) {
            tracing::info!(
                target: "whitenoise::scheduler::key_package_maintenance",
                "Account {} has no live canonical key package, publishing new package",
                account.pubkey.to_hex(),
            );
            return publish_new_key_package(whitenoise, account).await;
        }

        tracing::debug!(
            target: "whitenoise::scheduler::key_package_maintenance",
            "Account {} has {} live compatible tracked key package(s), none expired",
            account.pubkey.to_hex(),
            our_packages.len()
        );
        return MaintenanceResult::Fresh;
    }

    // Delete expired packages, publishing a replacement first if the state
    // left after deletion would not satisfy the current migration phase.
    let total_package_group_count = count_key_package_hash_groups(&our_packages);
    let non_expired_packages = find_non_expired_packages(&our_packages, &our_expired_packages);
    let needs_replacement_before_delete = !has_required_live_key_package_set(&non_expired_packages);
    rotate_expired_packages(
        whitenoise,
        account,
        our_expired_packages,
        total_package_group_count,
        needs_replacement_before_delete,
    )
    .await
}

fn filter_compatible_key_packages(
    packages: Vec<LivePublishedKeyPackage>,
) -> Vec<LivePublishedKeyPackage> {
    packages
        .into_iter()
        .filter(|package| {
            match validate_marmot_key_package_strict(&package.event, REQUIRED_MLS_CIPHERSUITE_TAG) {
                Ok(()) => true,
                Err(e) => {
                    tracing::debug!(
                        target: "whitenoise::scheduler::key_package_maintenance",
                        "Ignoring locally tracked key package {} because it is incompatible: {}",
                        package.event.id,
                        e
                    );
                    false
                }
            }
        })
        .collect()
}

/// Returns whole key package hash groups whose newest event is older than the maximum age.
fn find_expired_packages(packages: &[LivePublishedKeyPackage]) -> Vec<LivePublishedKeyPackage> {
    let now = Timestamp::now();
    let max_age_secs = KEY_PACKAGE_MAX_AGE.as_secs();
    let mut packages_by_hash_ref: HashMap<Vec<u8>, Vec<LivePublishedKeyPackage>> = HashMap::new();

    for package in packages {
        packages_by_hash_ref
            .entry(package.key_package_hash_ref.clone())
            .or_default()
            .push(package.clone());
    }

    packages_by_hash_ref
        .into_values()
        .filter(|package_group| {
            package_group
                .iter()
                .map(|package| package.event.created_at)
                .max()
                .is_some_and(|most_recent| {
                    now.as_secs().saturating_sub(most_recent.as_secs()) >= max_age_secs
                })
        })
        .flatten()
        .collect()
}

fn count_key_package_hash_groups(packages: &[LivePublishedKeyPackage]) -> usize {
    packages
        .iter()
        .map(|package| &package.key_package_hash_ref)
        .collect::<HashSet<_>>()
        .len()
}

fn has_live_canonical_key_package(packages: &[LivePublishedKeyPackage]) -> bool {
    packages
        .iter()
        .any(|package| package.event.kind == MLS_KEY_PACKAGE_KIND)
}

fn has_required_live_key_package_set(packages: &[LivePublishedKeyPackage]) -> bool {
    has_live_canonical_key_package(packages)
}

fn find_non_expired_packages(
    packages: &[LivePublishedKeyPackage],
    expired_packages: &[LivePublishedKeyPackage],
) -> Vec<LivePublishedKeyPackage> {
    let expired_hash_refs: HashSet<&Vec<u8>> = expired_packages
        .iter()
        .map(|package| &package.key_package_hash_ref)
        .collect();

    packages
        .iter()
        .filter(|package| !expired_hash_refs.contains(&package.key_package_hash_ref))
        .cloned()
        .collect()
}

/// Returns relay key packages that this device published and still has usable local state for.
#[perf_instrument("scheduled::key_package_maintenance")]
async fn find_live_published_key_packages(
    whitenoise: &Whitenoise,
    account: &Account,
    packages: Vec<Event>,
) -> Result<Vec<LivePublishedKeyPackage>, WhitenoiseError> {
    let session = whitenoise.require_session(&account.pubkey)?;
    let mut our_packages = Vec::new();

    for event in packages {
        match session
            .repos
            .published_key_packages
            .find_by_event_id(&event.id.to_hex())
            .await?
        {
            Some(pkg) if !pkg.key_material_deleted && pkg.consumed_at.is_none() => our_packages
                .push(LivePublishedKeyPackage {
                    event,
                    key_package_hash_ref: pkg.key_package_hash_ref,
                }),
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

    let Some(session) = whitenoise.session(&account.pubkey) else {
        return MaintenanceResult::Skipped;
    };

    match session.key_packages().publish().await {
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
/// If the account would be left without the live package set required by the current migration
/// phase after deletion, a new pair is published first to avoid a gap. Otherwise, only the expired
/// packages are deleted without republishing.
#[perf_instrument("scheduled::key_package_maintenance")]
async fn rotate_expired_packages(
    whitenoise: &Whitenoise,
    account: &Account,
    expired_packages: Vec<LivePublishedKeyPackage>,
    total_package_group_count: usize,
    needs_replacement_before_delete: bool,
) -> MaintenanceResult {
    let expired_group_count = count_key_package_hash_groups(&expired_packages);
    let non_expired_group_count = total_package_group_count.saturating_sub(expired_group_count);
    let expired_events: Vec<Event> = expired_packages
        .into_iter()
        .map(|package| package.event)
        .collect();

    tracing::info!(
        target: "whitenoise::scheduler::key_package_maintenance",
        "Account {} has {} expired key package group(s) ({} event(s)) and {} non-expired group(s), cleaning up",
        account.pubkey.to_hex(),
        expired_group_count,
        expired_events.len(),
        non_expired_group_count
    );

    let Some(session) = whitenoise.session(&account.pubkey) else {
        return MaintenanceResult::Skipped;
    };

    // Publish a new canonical key package if deleting the expired groups would
    // leave no live canonical key package behind.
    if needs_replacement_before_delete {
        if let Err(e) = session.key_packages().publish().await {
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
    match session
        .key_packages()
        .delete_batch(expired_events, false, 1)
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
    use crate::whitenoise::key_packages::MLS_PROPOSALS_TAG_KEY;
    use crate::whitenoise::relays::Relay;
    use crate::whitenoise::test_utils::{
        assert_obsolete_mls_artifacts_absent, create_mock_whitenoise, remove_obsolete_mls_artifacts,
    };
    use nostr_sdk::prelude::*;

    fn live_package(event: Event, key_package_hash_ref: Vec<u8>) -> LivePublishedKeyPackage {
        LivePublishedKeyPackage {
            event,
            key_package_hash_ref,
        }
    }

    fn compatible_key_package_event(content: &str) -> Event {
        let keys = Keys::generate();
        let tags = vec![
            Tag::custom(TagKind::MlsCiphersuite, vec![REQUIRED_MLS_CIPHERSUITE_TAG]),
            Tag::custom(TagKind::MlsExtensions, vec!["0x000a", "0xf2ee"]),
            Tag::custom(
                TagKind::Custom(MLS_PROPOSALS_TAG_KEY.into()),
                vec!["0x000a"],
            ),
            Tag::custom(TagKind::Custom("encoding".into()), vec!["base64"]),
        ];

        EventBuilder::new(MLS_KEY_PACKAGE_KIND, content)
            .tag(Tag::identifier("maintenance-compatible-test"))
            .tags(tags)
            .sign_with_keys(&keys)
            .unwrap()
    }

    fn key_package_event(kind: Kind) -> Event {
        EventBuilder::new(kind, "test_content")
            .sign_with_keys(&Keys::generate())
            .unwrap()
    }

    #[test]
    fn test_has_live_canonical_key_package_requires_canonical_kind() {
        let canonical = live_package(key_package_event(MLS_KEY_PACKAGE_KIND), vec![1]);
        let other_kind = live_package(key_package_event(Kind::TextNote), vec![2]);
        let other_kind_only = std::slice::from_ref(&other_kind);

        assert!(!has_live_canonical_key_package(&[]));
        assert!(!has_live_canonical_key_package(other_kind_only));
        assert!(has_live_canonical_key_package(&[other_kind, canonical]));
    }

    #[test]
    fn test_required_live_key_package_set_requires_canonical_package() {
        let canonical = live_package(key_package_event(MLS_KEY_PACKAGE_KIND), vec![1]);
        let other_kind = live_package(key_package_event(Kind::TextNote), vec![2]);

        assert!(!has_required_live_key_package_set(&[]));
        assert!(has_required_live_key_package_set(std::slice::from_ref(
            &canonical
        )));
        assert!(!has_required_live_key_package_set(std::slice::from_ref(
            &other_kind
        )));
        assert!(has_required_live_key_package_set(&[canonical, other_kind]));
    }

    #[test]
    fn test_find_non_expired_packages_filters_whole_expired_hash_groups() {
        let expired_canonical = live_package(key_package_event(MLS_KEY_PACKAGE_KIND), vec![1]);
        let expired_sibling = live_package(key_package_event(MLS_KEY_PACKAGE_KIND), vec![1]);
        let fresh_canonical = live_package(key_package_event(MLS_KEY_PACKAGE_KIND), vec![2]);
        let fresh_sibling = live_package(key_package_event(MLS_KEY_PACKAGE_KIND), vec![2]);
        let packages = vec![
            expired_canonical.clone(),
            expired_sibling.clone(),
            fresh_canonical.clone(),
            fresh_sibling.clone(),
        ];
        let expired_packages = vec![expired_canonical, expired_sibling];

        let non_expired = find_non_expired_packages(&packages, &expired_packages);

        assert_eq!(non_expired.len(), 2);
        assert!(
            non_expired
                .iter()
                .all(|package| package.key_package_hash_ref == vec![2])
        );
    }

    /// Publishes a malformed canonical key package event for testing outdated
    /// package rotation. The event is intentionally not backed by local
    /// key-package material; the scheduler should ignore relay events that are
    /// not both compatible and locally tracked.
    async fn publish_outdated_key_package(
        whitenoise: &Whitenoise,
        account: &crate::whitenoise::accounts::Account,
        relays: &[Relay],
    ) -> Result<EventId, crate::whitenoise::error::WhitenoiseError> {
        let nsec = whitenoise.export_account_nsec(account).await?;
        let secret_key =
            SecretKey::from_bech32(&nsec).map_err(|e| WhitenoiseError::Internal(e.to_string()))?;
        let keys = Keys::new(secret_key);

        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "outdated-key-package")
            .tag(Tag::identifier("maintenance-outdated-test"))
            .tag(Tag::custom(
                TagKind::MlsCiphersuite,
                vec![REQUIRED_MLS_CIPHERSUITE_TAG],
            ))
            .sign_with_keys(&keys)
            .map_err(|e| WhitenoiseError::Internal(e.to_string()))?;

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

    async fn mark_all_key_package_groups_deleted(
        whitenoise: &Whitenoise,
        account: &crate::whitenoise::accounts::Account,
        events: &[Event],
    ) {
        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let mut seen_hash_refs = std::collections::HashSet::new();

        for event in events {
            let tracked = session
                .repos
                .published_key_packages
                .find_by_event_id(&event.id.to_hex())
                .await
                .unwrap()
                .unwrap();
            if !seen_hash_refs.insert(tracked.key_package_hash_ref.clone()) {
                continue;
            }
            session
                .key_packages()
                .delete_tracked_key_material(&tracked)
                .await
                .unwrap();
            session
                .repos
                .published_key_packages
                .mark_key_material_deleted_by_hash_ref(&tracked.key_package_hash_ref)
                .await
                .unwrap();
        }
    }

    async fn mark_all_key_package_groups_consumed(
        whitenoise: &Whitenoise,
        account: &crate::whitenoise::accounts::Account,
        events: &[Event],
    ) {
        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let mut seen_hash_refs = std::collections::HashSet::new();

        for event in events {
            let tracked = session
                .repos
                .published_key_packages
                .find_by_event_id(&event.id.to_hex())
                .await
                .unwrap()
                .unwrap();
            if !seen_hash_refs.insert(tracked.key_package_hash_ref) {
                continue;
            }
            session
                .repos
                .published_key_packages
                .mark_consumed(&event.id.to_hex())
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_execute_republishes_when_local_key_material_is_missing() {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account = whitenoise.create_identity().await.unwrap();
        let artifacts = remove_obsolete_mls_artifacts(&account, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);
        tokio::time::sleep(Duration::from_millis(500)).await;

        let before = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .key_packages()
            .fetch_all()
            .await
            .unwrap();
        assert_eq!(
            before.len(),
            1,
            "Darkmatter-only sessions should start with a canonical key package"
        );

        mark_all_key_package_groups_deleted(&whitenoise, &account, &before).await;
        assert_obsolete_mls_artifacts_absent(&artifacts);

        // Validate the precondition for this regression: the relay event still
        // exists, but this device no longer has any live local state for it.
        let live_before = find_live_published_key_packages(&whitenoise, &account, before)
            .await
            .unwrap();
        assert!(
            live_before.is_empty(),
            "No relay key package should remain tracked as live after deleting local key material"
        );

        let task = KeyPackageMaintenance;
        task.execute(whitenoise.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_obsolete_mls_artifacts_absent(&artifacts);

        let after = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .key_packages()
            .fetch_all()
            .await
            .unwrap();
        let live_after = find_live_published_key_packages(&whitenoise, &account, after)
            .await
            .unwrap();
        assert!(
            !live_after.is_empty(),
            "Maintenance should publish a fresh key package when relay packages no longer match live local state"
        );
    }

    #[tokio::test]
    async fn test_execute_republishes_when_canonical_key_package_is_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account = whitenoise.create_identity().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let before = session.key_packages().fetch_all().await.unwrap();
        assert_eq!(
            before.len(),
            1,
            "Darkmatter-only sessions should start with a canonical key package"
        );

        let canonical = before
            .into_iter()
            .find(|event| event.kind == MLS_KEY_PACKAGE_KIND)
            .expect("canonical event exists");
        assert!(
            session
                .key_packages()
                .delete(&canonical.id, false)
                .await
                .unwrap(),
            "test setup should delete the canonical key package from relays"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;

        let missing_canonical = session.key_packages().fetch_all().await.unwrap();
        assert!(
            missing_canonical.is_empty(),
            "precondition: relay should have no canonical key package"
        );

        let live_before =
            find_live_published_key_packages(&whitenoise, &account, missing_canonical.clone())
                .await
                .unwrap();
        assert!(
            live_before.is_empty(),
            "precondition: no relay key package should count as live"
        );

        let task = KeyPackageMaintenance;
        task.execute(whitenoise.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let after = session.key_packages().fetch_all().await.unwrap();
        let live_after = find_live_published_key_packages(&whitenoise, &account, after)
            .await
            .unwrap();
        assert!(
            live_after
                .iter()
                .any(|package| package.event.kind == MLS_KEY_PACKAGE_KIND),
            "Maintenance should republish the missing canonical key package"
        );
    }

    #[tokio::test]
    async fn test_execute_preserves_darkmatter_canonical_key_package() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account = whitenoise.create_identity().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let session = whitenoise.require_session(&account.pubkey).unwrap();
        assert!(
            session.has_marmot_session(),
            "test precondition: local sessions publish Darkmatter v2 canonical packages"
        );

        let before = session.key_packages().fetch_all().await.unwrap();
        assert_eq!(
            before.len(),
            1,
            "Darkmatter-only sessions should start with a canonical key package"
        );

        let canonical_only = session.key_packages().fetch_all().await.unwrap();
        assert!(
            canonical_only
                .iter()
                .any(|event| event.kind == MLS_KEY_PACKAGE_KIND),
            "precondition: relay should still have a canonical key package"
        );

        let task = KeyPackageMaintenance;
        task.execute(whitenoise.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let after = session.key_packages().fetch_all().await.unwrap();
        let live_after = find_live_published_key_packages(&whitenoise, &account, after)
            .await
            .unwrap();
        assert!(
            live_after
                .iter()
                .any(|package| package.event.kind == MLS_KEY_PACKAGE_KIND),
            "Maintenance should preserve a live canonical key package"
        );
    }

    #[tokio::test]
    async fn test_execute_republishes_when_only_consumed_key_packages_remain() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account = whitenoise.create_identity().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let before = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .key_packages()
            .fetch_all()
            .await
            .unwrap();
        assert_eq!(
            before.len(),
            1,
            "Darkmatter-only sessions should start with a canonical key package"
        );

        mark_all_key_package_groups_consumed(&whitenoise, &account, &before).await;

        // Validate the precondition for this regression: the relay event still
        // exists, but the tracked package has already been consumed locally.
        let live_before = find_live_published_key_packages(&whitenoise, &account, before)
            .await
            .unwrap();
        assert!(
            live_before.is_empty(),
            "Consumed key packages should not count as live local state"
        );

        let task = KeyPackageMaintenance;
        task.execute(whitenoise.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let after = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .key_packages()
            .fetch_all()
            .await
            .unwrap();
        let live_after = find_live_published_key_packages(&whitenoise, &account, after)
            .await
            .unwrap();
        assert!(
            !live_after.is_empty(),
            "Maintenance should publish a fresh key package when only consumed packages remain"
        );
    }

    #[tokio::test]
    async fn test_execute_republishes_when_outdated_and_only_consumed_valid_packages_remain() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account = whitenoise.create_identity().await.unwrap();
        let kp_relays = account
            .key_package_relays(&whitenoise.shared)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let before = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .key_packages()
            .fetch_all()
            .await
            .unwrap();
        assert_eq!(
            before.len(),
            1,
            "Darkmatter-only sessions should start with a canonical key package"
        );

        mark_all_key_package_groups_consumed(&whitenoise, &account, &before).await;

        publish_outdated_key_package(&whitenoise, &account, &kp_relays)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let packages = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .key_packages()
            .fetch_all()
            .await
            .unwrap();
        assert_eq!(
            packages.len(),
            2,
            "Should have canonical Darkmatter key package plus outdated canonical key package"
        );

        let live_before = find_live_published_key_packages(&whitenoise, &account, packages.clone())
            .await
            .unwrap();
        assert!(
            live_before.is_empty(),
            "Consumed valid packages plus outdated packages should still count as no live local state"
        );

        let task = KeyPackageMaintenance;
        task.execute(whitenoise.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let after = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .key_packages()
            .fetch_all()
            .await
            .unwrap();
        let live_after = find_live_published_key_packages(&whitenoise, &account, after)
            .await
            .unwrap();
        assert!(
            !live_after.is_empty(),
            "Maintenance should publish a fresh key package when outdated packages coexist with only consumed valid ones"
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

        let task = KeyPackageMaintenance;
        let result = task.execute(whitenoise.clone()).await;

        // Should succeed - just logs "No accounts found, skipping"
        assert!(result.is_ok());
    }

    #[test]
    fn test_find_expired_packages_returns_old_packages() {
        let keys = nostr_sdk::Keys::generate();

        // Create an event with a timestamp 31 days in the past
        let old_timestamp = nostr_sdk::Timestamp::now() - Duration::from_secs(31 * 24 * 60 * 60);
        let old_event = nostr_sdk::EventBuilder::new(MLS_KEY_PACKAGE_KIND, "old")
            .custom_created_at(old_timestamp)
            .sign_with_keys(&keys)
            .unwrap();

        // Create an event with a fresh timestamp
        let fresh_event = nostr_sdk::EventBuilder::new(MLS_KEY_PACKAGE_KIND, "fresh")
            .sign_with_keys(&keys)
            .unwrap();

        let packages = vec![
            live_package(old_event.clone(), vec![1]),
            live_package(fresh_event.clone(), vec![2]),
        ];
        let expired = find_expired_packages(&packages);

        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].event.id, old_event.id);
    }

    #[test]
    fn test_find_expired_packages_returns_empty_when_all_fresh() {
        let keys = nostr_sdk::Keys::generate();

        let fresh1 = nostr_sdk::EventBuilder::new(MLS_KEY_PACKAGE_KIND, "fresh1")
            .sign_with_keys(&keys)
            .unwrap();
        let fresh2 = nostr_sdk::EventBuilder::new(MLS_KEY_PACKAGE_KIND, "fresh2")
            .sign_with_keys(&keys)
            .unwrap();

        let packages = vec![live_package(fresh1, vec![1]), live_package(fresh2, vec![2])];
        let expired = find_expired_packages(&packages);
        assert!(expired.is_empty());
    }

    #[test]
    fn test_find_expired_packages_handles_empty_input() {
        let expired = find_expired_packages(&[]);
        assert!(expired.is_empty());
    }

    #[test]
    fn test_find_expired_packages_keeps_group_when_sibling_is_fresh() {
        let keys = nostr_sdk::Keys::generate();
        let old_timestamp = nostr_sdk::Timestamp::now() - Duration::from_secs(31 * 24 * 60 * 60);

        let old_event = nostr_sdk::EventBuilder::new(MLS_KEY_PACKAGE_KIND, "old")
            .custom_created_at(old_timestamp)
            .sign_with_keys(&keys)
            .unwrap();
        let fresh_event = nostr_sdk::EventBuilder::new(MLS_KEY_PACKAGE_KIND, "fresh")
            .sign_with_keys(&keys)
            .unwrap();

        let packages = vec![
            live_package(old_event, vec![1, 2, 3]),
            live_package(fresh_event, vec![1, 2, 3]),
        ];
        let expired = find_expired_packages(&packages);

        assert!(
            expired.is_empty(),
            "A hash_ref group should not expire while any sibling is fresh"
        );
    }

    #[test]
    fn test_find_expired_packages_returns_entire_expired_group() {
        let keys = nostr_sdk::Keys::generate();
        let old_timestamp = nostr_sdk::Timestamp::now() - Duration::from_secs(31 * 24 * 60 * 60);

        let old_event_a = nostr_sdk::EventBuilder::new(MLS_KEY_PACKAGE_KIND, "old-a")
            .custom_created_at(old_timestamp)
            .sign_with_keys(&keys)
            .unwrap();
        let old_event_b = nostr_sdk::EventBuilder::new(MLS_KEY_PACKAGE_KIND, "old-b")
            .custom_created_at(old_timestamp)
            .sign_with_keys(&keys)
            .unwrap();

        let packages = vec![
            live_package(old_event_a, vec![1, 2, 3]),
            live_package(old_event_b, vec![1, 2, 3]),
        ];
        let expired = find_expired_packages(&packages);

        assert_eq!(
            expired.len(),
            2,
            "An expired hash_ref group should be rotated as a whole"
        );
    }

    #[test]
    fn test_count_key_package_hash_groups_deduplicates_siblings() {
        let keys = nostr_sdk::Keys::generate();
        let canonical = nostr_sdk::EventBuilder::new(MLS_KEY_PACKAGE_KIND, "canonical")
            .sign_with_keys(&keys)
            .unwrap();
        let sibling = nostr_sdk::EventBuilder::new(MLS_KEY_PACKAGE_KIND, "sibling")
            .sign_with_keys(&keys)
            .unwrap();
        let separate = nostr_sdk::EventBuilder::new(MLS_KEY_PACKAGE_KIND, "separate")
            .sign_with_keys(&keys)
            .unwrap();

        let packages = vec![
            live_package(canonical, vec![1, 2, 3]),
            live_package(sibling, vec![1, 2, 3]),
            live_package(separate, vec![4, 5, 6]),
        ];

        assert_eq!(count_key_package_hash_groups(&packages), 2);
    }

    #[test]
    fn test_filter_compatible_key_packages_requires_self_remove() {
        let compatible = compatible_key_package_event("Y29tcGF0aWJsZQ==");
        let incompatible = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "aW5jb21wYXRpYmxl")
            .tag(Tag::identifier("maintenance-incompatible-test"))
            .tags(vec![
                Tag::custom(TagKind::MlsCiphersuite, vec![REQUIRED_MLS_CIPHERSUITE_TAG]),
                Tag::custom(TagKind::MlsExtensions, vec!["0x000a", "0xf2ee"]),
                Tag::custom(TagKind::Custom("encoding".into()), vec!["base64"]),
            ])
            .sign_with_keys(&Keys::generate())
            .unwrap();

        let filtered = filter_compatible_key_packages(vec![
            live_package(compatible.clone(), vec![1]),
            live_package(incompatible, vec![2]),
        ]);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].event.id, compatible.id);
    }

    #[tokio::test]
    async fn test_filter_compatible_key_packages_accepts_darkmatter_v2() {
        let keys = Keys::generate();
        let event =
            crate::marmot::key_packages::testsupport::adapter_key_package_event(&keys, None, false)
                .await;

        let filtered = filter_compatible_key_packages(vec![live_package(event.clone(), vec![1])]);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].event.id, event.id);
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

use async_trait::async_trait;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::integration_tests::test_cases::shared::{CreateGroupTestCase, WaitForWelcomeTestCase};
use crate::whitenoise::scheduled_tasks::{ConsumedKeyPackageCleanup, Task};

/// Verifies the full key package lifecycle:
/// 1. Publishing a KP tracks it in `published_key_packages`
/// 2. Receiving a Welcome marks it as consumed
/// 3. Maintenance task cleans up local key material after the quiet period
pub struct KeyPackageLifecycleTestCase {
    creator_name: String,
    member_name: String,
}

impl KeyPackageLifecycleTestCase {
    pub fn new(creator: &str, member: &str) -> Self {
        Self {
            creator_name: creator.to_string(),
            member_name: member.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for KeyPackageLifecycleTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!("Testing full key package lifecycle...");

        // ============================================================
        // Phase 1: Create accounts and verify KP tracking
        // ============================================================
        let creator = context.whitenoise.create_identity().await?;
        context.add_account(&self.creator_name, creator.clone());
        tracing::info!("✓ Created creator: {}", creator.pubkey.to_hex());

        let member = context.whitenoise.create_identity().await?;
        context.add_account(&self.member_name, member.clone());
        tracing::info!("✓ Created member: {}", member.pubkey.to_hex());

        // Wait for initial key package publishing to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // Verify the member has at least one published KP tracked in the DB
        let member_kps = context
            .whitenoise
            .fetch_all_key_packages_for_account(&member)
            .await?;
        assert!(
            !member_kps.is_empty(),
            "Member should have at least one key package on relays"
        );

        // Look up the tracked record for one of the member's KPs
        let kp_event_id = member_kps[0].id.to_hex();
        let tracked = context
            .whitenoise
            .find_published_key_package_for_testing(&member.pubkey, &kp_event_id)
            .await?;
        assert!(
            tracked.is_some(),
            "Published KP should be tracked in published_key_packages table"
        );
        let tracked = tracked.unwrap();
        assert!(
            tracked.consumed_at.is_none(),
            "Freshly published KP should not be consumed yet"
        );
        assert!(
            !tracked.key_material_deleted,
            "Freshly published KP should have key material available"
        );
        tracing::info!(
            "✓ Published KP is tracked with consumed_at=None, key_material_deleted=false"
        );

        // ============================================================
        // Phase 2: Create a group to trigger Welcome → KP consumption
        // ============================================================
        tracing::info!("Creating group to trigger Welcome and KP consumption...");

        CreateGroupTestCase::basic()
            .with_name("lifecycle_group")
            .with_members(&self.creator_name, vec![&self.member_name])
            .execute(context)
            .await?;

        // Wait for the member to receive and process the Welcome
        WaitForWelcomeTestCase::for_account(&self.member_name, "lifecycle_group")
            .execute(context)
            .await?;

        // The background `rotate_key_package` marks the KP consumed asynchronously.
        // Poll the DB until consumed_at is set.
        let member_pubkey = member.pubkey;
        let kp_event_id_clone = kp_event_id.clone();
        let wn = context.whitenoise;

        retry_default(
            || {
                let kp_event_id = kp_event_id_clone.clone();
                async move {
                    let pkg = wn
                        .find_published_key_package_for_testing(&member_pubkey, &kp_event_id)
                        .await?;

                    match pkg {
                        Some(p) if p.consumed_at.is_some() => Ok(()),
                        _ => Err(WhitenoiseError::Other(anyhow::anyhow!(
                            "KP not yet marked as consumed"
                        ))),
                    }
                }
            },
            "KP marked as consumed after Welcome",
        )
        .await?;

        tracing::info!("✓ KP marked as consumed after Welcome processing");

        // ============================================================
        // Phase 3: Run maintenance and verify cleanup
        // ============================================================
        tracing::info!("Running maintenance cleanup...");

        // Backdate consumed_at so the quiet period check passes immediately,
        // avoiding a 35-second real-time wait in the test suite.
        context
            .whitenoise
            .backdate_consumed_at_for_testing(&member.pubkey, &kp_event_id, 60)
            .await?;

        // Run the cleanup task
        let task = ConsumedKeyPackageCleanup;
        task.execute(context.whitenoise).await?;

        // Verify key_material_deleted is now set
        let after_cleanup = context
            .whitenoise
            .find_published_key_package_for_testing(&member.pubkey, &kp_event_id)
            .await?;

        let after_cleanup = after_cleanup.expect("KP record should still exist (never deleted)");
        assert!(
            after_cleanup.key_material_deleted,
            "Key material should be marked as deleted after maintenance cleanup"
        );
        assert!(
            after_cleanup.consumed_at.is_some(),
            "consumed_at should still be set"
        );

        tracing::info!("✓ Maintenance task cleaned up local key material");
        tracing::info!("✓ Full key package lifecycle verified: publish → consume → cleanup");

        Ok(())
    }
}

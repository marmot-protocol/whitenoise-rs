use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::integration_tests::{
    core::*,
    test_cases::{block_filters_messages::*, mute_list::*, shared::*},
};
use crate::{Whitenoise, WhitenoiseError};

/// Settle delay between mute-list mutations. Two kind-10000 events published
/// in the same wall-clock second collide on `created_at` and the relay
/// rejects the republish with "replaced: have newer event". Mirrors
/// `MuteListScenario`'s delay.
const MUTE_LIST_SETTLE_DELAY: Duration = Duration::from_secs(2);

/// End-to-end proof that the `is_blocked` marker is stamped at message-ingest
/// time and frozen across block/unblock — the behavior the unit tests cannot
/// exercise because it spans the relay → MLS → `cache_chat_message` pipeline.
///
/// A regular two-member group is used: the `is_blocked` stamp is group-type
/// agnostic (the DM-vs-group *projection* asymmetry is covered by the
/// `aggregated_messages` unit tests). The scenario verifies, from the
/// blocker's own cached view:
///
/// - a message received *before* a block is not stamped;
/// - a message received *while* the author is blocked is stamped, and the
///   earlier message stays unstamped (forward-looking rule);
/// - a message received *after* an unblock is not stamped, and the
///   during-block message keeps its stamp (forward-looking rule).
pub struct BlockFiltersMessagesScenario {
    context: ScenarioContext,
}

impl BlockFiltersMessagesScenario {
    pub fn new(whitenoise: Arc<Whitenoise>) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for BlockFiltersMessagesScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        CreateAccountsTestCase::with_names(vec!["blk_blocker", "blk_peer"])
            .execute(&mut self.context)
            .await?;

        CreateGroupTestCase::basic()
            .with_name("block_filter_group")
            .with_members("blk_blocker", vec!["blk_peer"])
            .execute(&mut self.context)
            .await?;

        WaitForWelcomeTestCase::for_account("blk_peer", "block_filter_group")
            .execute(&mut self.context)
            .await?;

        // 1. Peer sends before any block — the blocker's copy is unblocked.
        SendMessageTestCase::basic()
            .with_sender("blk_peer")
            .with_group("block_filter_group")
            .with_content("before the block")
            .with_message_id_key("msg_before")
            .execute(&mut self.context)
            .await?;
        VerifyMessageBlockStateTestCase::new(
            "blk_blocker",
            "block_filter_group",
            "msg_before",
            false,
        )
        .execute(&mut self.context)
        .await?;

        // 2. Blocker blocks the peer.
        let peer_pubkey = self.context.get_account("blk_peer")?.pubkey;
        BlockUserTestCase::new("blk_blocker", peer_pubkey)
            .execute(&mut self.context)
            .await?;

        // 3. Peer sends while blocked — the blocker's copy is stamped, and
        //    the pre-block message stays unstamped (forward-looking rule).
        SendMessageTestCase::basic()
            .with_sender("blk_peer")
            .with_group("block_filter_group")
            .with_content("during the block")
            .with_message_id_key("msg_during")
            .execute(&mut self.context)
            .await?;
        VerifyMessageBlockStateTestCase::new(
            "blk_blocker",
            "block_filter_group",
            "msg_during",
            true,
        )
        .execute(&mut self.context)
        .await?;
        VerifyMessageBlockStateTestCase::new(
            "blk_blocker",
            "block_filter_group",
            "msg_before",
            false,
        )
        .execute(&mut self.context)
        .await?;

        tokio::time::sleep(MUTE_LIST_SETTLE_DELAY).await;

        // 4. Blocker unblocks the peer.
        UnblockUserTestCase::new("blk_blocker", peer_pubkey)
            .execute(&mut self.context)
            .await?;

        // 5. Peer sends after the unblock — the blocker's copy is unblocked,
        //    and the during-block message keeps its stamp (forward-looking).
        SendMessageTestCase::basic()
            .with_sender("blk_peer")
            .with_group("block_filter_group")
            .with_content("after the unblock")
            .with_message_id_key("msg_after")
            .execute(&mut self.context)
            .await?;
        VerifyMessageBlockStateTestCase::new(
            "blk_blocker",
            "block_filter_group",
            "msg_after",
            false,
        )
        .execute(&mut self.context)
        .await?;
        VerifyMessageBlockStateTestCase::new(
            "blk_blocker",
            "block_filter_group",
            "msg_during",
            true,
        )
        .execute(&mut self.context)
        .await?;

        tracing::info!("✓ Block-filters-messages scenario completed with:");
        tracing::info!("  • Pre-block message → not stamped");
        tracing::info!("  • During-block message → stamped; pre-block stays visible");
        tracing::info!("  • Post-unblock message → not stamped; during-block stays stamped");

        Ok(())
    }
}

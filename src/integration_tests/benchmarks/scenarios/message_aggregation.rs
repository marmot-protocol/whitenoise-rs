use std::time::Duration;

use async_trait::async_trait;

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::test_cases::FetchAggregatedMessagesBenchmark;
use crate::integration_tests::benchmarks::{BenchmarkConfig, BenchmarkScenario, BenchmarkTestCase};
use crate::integration_tests::core::{ScenarioContext, TestCase};
use crate::integration_tests::test_cases::chat_media_upload::{
    SendMessageWithMediaTestCase, UploadChatImageTestCase,
};
use crate::integration_tests::test_cases::shared::{
    CreateAccountsTestCase, CreateGroupTestCase, DeleteMessageTestCase, SendMessageTestCase,
};

/// Configuration for message aggregation benchmark dataset
#[derive(Debug, Clone)]
pub struct MessageDatasetConfig {
    /// Number of regular text messages to create
    pub regular_messages: usize,
    /// Number of messages with media attachments
    pub media_messages: usize,
    /// Total number of reactions to distribute across messages
    pub reactions_count: usize,
    /// Indices of messages to delete (relative to regular messages)
    pub deletions: Vec<usize>,
    /// Indices of messages to reply to (relative to regular messages)
    pub replies: Vec<usize>,
}

impl Default for MessageDatasetConfig {
    fn default() -> Self {
        Self {
            regular_messages: 100,
            media_messages: 15,
            reactions_count: 30,
            deletions: vec![10, 15, 22, 35, 40, 52, 60, 68, 73, 74],
            replies: vec![
                5, 8, 12, 18, 20, 21, 25, 30, 33, 38, 42, 45, 50, 55, 58, 63, 67, 69, 70, 72,
            ],
        }
    }
}

impl MessageDatasetConfig {
    /// Calculate total number of messages that will be created
    pub fn total_events(&self) -> usize {
        // Regular messages + media messages + 2 group access test messages + reactions + deletions + replies
        self.regular_messages
            + self.media_messages
            + 2 // bob and charlie test messages
            + self.reactions_count
            + self.deletions.len()
            + self.replies.len()
    }
}

pub struct MessageAggregationBenchmark {
    test_case: FetchAggregatedMessagesBenchmark,
    dataset_config: MessageDatasetConfig,
}

impl MessageAggregationBenchmark {
    pub fn new(
        test_case: FetchAggregatedMessagesBenchmark,
        dataset_config: MessageDatasetConfig,
    ) -> Self {
        Self {
            test_case,
            dataset_config,
        }
    }
}

impl Default for MessageAggregationBenchmark {
    fn default() -> Self {
        Self::new(
            FetchAggregatedMessagesBenchmark::new("alice", "benchmark_group"),
            MessageDatasetConfig::default(),
        )
    }
}

#[async_trait]
impl BenchmarkScenario for MessageAggregationBenchmark {
    fn name(&self) -> &str {
        "Message Aggregation Performance"
    }

    fn config(&self) -> BenchmarkConfig {
        BenchmarkConfig {
            iterations: 100,
            warmup_iterations: 10,
            cooldown_between_iterations: Duration::from_millis(10),
        }
    }

    async fn setup(&mut self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let cfg = &self.dataset_config;

        tracing::info!("Setting up benchmark dataset...");
        tracing::info!(
            "  Configuration: {} regular, {} media, {} reactions, {} deletions, {} replies",
            cfg.regular_messages,
            cfg.media_messages,
            cfg.reactions_count,
            cfg.deletions.len(),
            cfg.replies.len()
        );

        // 1. Create accounts and group
        CreateAccountsTestCase::with_names(vec!["alice", "bob", "charlie"])
            .run(context)
            .await?;

        CreateGroupTestCase::basic()
            .with_name("benchmark_group")
            .with_members("alice", vec!["bob", "charlie"])
            .run(context)
            .await?;

        // Wait for welcome invitations to be sent and processed
        // Note: MLS membership is auto-finalized when welcome is received,
        // so members can participate immediately without explicit accept
        tracing::info!("Waiting for welcome invitations to be processed...");
        tokio::time::sleep(Duration::from_millis(1000)).await;

        // Ensure bob and charlie have proper group access by having them send test messages
        tracing::info!("Verifying group access for bob and charlie...");
        SendMessageTestCase::basic()
            .with_sender("bob")
            .with_group("benchmark_group")
            .with_content("Bob group access test")
            .with_message_id_key("bob_test")
            .run(context)
            .await?;

        SendMessageTestCase::basic()
            .with_sender("charlie")
            .with_group("benchmark_group")
            .with_content("Charlie group access test")
            .with_message_id_key("charlie_test")
            .run(context)
            .await?;

        // Small delay for message propagation
        tokio::time::sleep(Duration::from_millis(1000)).await;

        tracing::info!("✓ Accounts and group created, invitations accepted, group access verified");

        // 3. Send regular messages
        tracing::info!("Sending {} regular messages...", cfg.regular_messages);
        for i in 0..cfg.regular_messages {
            SendMessageTestCase::basic()
                .with_sender("alice")
                .with_group("benchmark_group")
                .with_content(&format!("Message {}", i))
                .with_message_id_key(&format!("msg_{}", i))
                .run(context)
                .await?;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // 4. Upload media once and send messages referencing it
        if cfg.media_messages > 0 {
            tracing::info!(
                "Uploading media and sending {} media messages...",
                cfg.media_messages
            );
            UploadChatImageTestCase::basic()
                .with_account("alice")
                .with_group("benchmark_group")
                .run(context)
                .await?;

            // Note: Using default content "Check out this image! 📸" for all
            for _i in 0..cfg.media_messages {
                SendMessageWithMediaTestCase::new("alice", "benchmark_group")
                    .run(context)
                    .await?;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        // 5. Add reactions distributed across messages
        if cfg.reactions_count > 0 {
            // Distribute reactions across the first N messages (or all if fewer)
            let messages_to_react_to = cfg.reactions_count.min(cfg.regular_messages);
            tracing::info!(
                "Adding {} reactions across {} messages...",
                cfg.reactions_count,
                messages_to_react_to
            );

            let emojis = ["👍", "❤️", "🎉", "🔥", "👀"];
            let mut reactions_added = 0;
            let mut emoji_idx = 0;

            for msg_idx in 0..messages_to_react_to {
                if reactions_added >= cfg.reactions_count {
                    break;
                }

                let msg_id = context.get_message_id(&format!("msg_{}", msg_idx))?.clone();
                let emoji = emojis[emoji_idx % emojis.len()];

                SendMessageTestCase::basic()
                    .with_sender("bob")
                    .with_group("benchmark_group")
                    .into_reaction(emoji, &msg_id)
                    .run(context)
                    .await?;

                reactions_added += 1;
                emoji_idx += 1;

                tokio::time::sleep(Duration::from_millis(10)).await;

                // Some messages get multiple reactions from different users
                if msg_idx % 3 == 0 && reactions_added < cfg.reactions_count {
                    SendMessageTestCase::basic()
                        .with_sender("charlie")
                        .with_group("benchmark_group")
                        .into_reaction(emojis[(emoji_idx + 1) % emojis.len()], &msg_id)
                        .run(context)
                        .await?;
                    reactions_added += 1;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }

        // 6. Delete specified messages
        if !cfg.deletions.is_empty() {
            tracing::info!("Deleting {} messages...", cfg.deletions.len());
            for &i in &cfg.deletions {
                if i < cfg.regular_messages {
                    let msg_id = format!("msg_{}", i);
                    DeleteMessageTestCase::new("alice", "benchmark_group", &msg_id)
                        .run(context)
                        .await?;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }

        // 7. Send reply messages
        if !cfg.replies.is_empty() {
            tracing::info!("Sending {} reply messages...", cfg.replies.len());
            for &i in &cfg.replies {
                if i < cfg.regular_messages {
                    let target_msg_id = context.get_message_id(&format!("msg_{}", i))?.clone();
                    SendMessageTestCase::basic()
                        .with_sender("charlie")
                        .with_group("benchmark_group")
                        .into_reply(&format!("Reply to message {}", i), &target_msg_id)
                        .run(context)
                        .await?;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }

        // 8. Wait for message propagation
        tracing::info!("Waiting for message propagation...");
        tokio::time::sleep(Duration::from_secs(2)).await;

        let total = cfg.total_events();
        tracing::info!("✓ Setup complete: {} total events created", total);
        tracing::info!(
            "  - {} regular text messages ({} + 2 group access tests)",
            cfg.regular_messages + 2,
            cfg.regular_messages
        );
        tracing::info!("  - {} messages with media attachments", cfg.media_messages);
        tracing::info!("  - {} reactions", cfg.reactions_count);
        tracing::info!("  - {} deleted messages", cfg.deletions.len());
        tracing::info!("  - {} reply messages", cfg.replies.len());

        Ok(())
    }

    async fn single_iteration(
        &self,
        context: &mut ScenarioContext,
    ) -> Result<Duration, WhitenoiseError> {
        self.test_case.run_iteration(context).await
    }
}

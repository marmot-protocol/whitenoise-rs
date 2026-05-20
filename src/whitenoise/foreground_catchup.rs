use std::collections::HashSet;

use mdk_core::prelude::GroupId;

use crate::perf_instrument;
use crate::whitenoise::{
    Whitenoise,
    accounts::Account,
    chat_list_streaming::{ChatListUpdate, ChatListUpdateTrigger},
    database::aggregated_messages::PaginationOptions,
    error::{Result, WhitenoiseError},
    message_streaming::{MessageUpdate, UpdateTrigger},
};

const FOREGROUND_MESSAGE_SNAPSHOT_LIMIT: u32 = 200;

impl Whitenoise {
    /// Catch the main app process up after foreground resume or notification tap.
    ///
    /// This is stronger than [`Self::ensure_all_subscriptions`]. The periodic
    /// ensure path may skip accounts whose relay planes look healthy; foreground
    /// resume intentionally refreshes subscriptions for every account so relays
    /// replay from the account's bounded `last_synced_at` anchor. That matters
    /// after iOS suspends the Runner process or after the Notification Service
    /// Extension fetched events in a separate process.
    ///
    /// Guarantees, best effort:
    /// - Discovery, inbox, and group subscriptions are refreshed with their
    ///   existing bounded `since` semantics.
    /// - One account's transient relay/signer/subscription failure is logged and
    ///   does not stop the remaining accounts from catching up.
    /// - Active chat-list and message streams in this process receive a replay
    ///   of the current DB snapshot where the existing stream types can carry it.
    ///
    /// What this does not guarantee:
    /// - Broadcasts emitted by the NSE process are not delivered into the Runner
    ///   process; this method rereads the shared DB instead.
    /// - Item/message streams cannot represent "the whole list is now exactly
    ///   this" or "this open timeline is empty". Call
    ///   [`Self::fetch_chat_list_snapshot`],
    ///   [`Self::fetch_archived_chat_list_snapshot`], and
    ///   [`Self::fetch_aggregated_messages_for_group`] after this method when a
    ///   Flutter view needs exact replacement rather than upsert-style refresh.
    ///
    /// Returns an error only when the shared account list cannot be read or
    /// another process-wide prerequisite fails. Per-account catch-up failures
    /// are logged and skipped.
    #[perf_instrument("whitenoise")]
    pub async fn resume_after_background(&self) -> Result<()> {
        let accounts = self.force_foreground_subscription_catch_up().await?;
        self.replay_foreground_stream_snapshots(&accounts).await;
        Ok(())
    }

    async fn force_foreground_subscription_catch_up(&self) -> Result<Vec<Account>> {
        if let Err(error) = self.sync_discovery_subscriptions().await {
            if Self::foreground_catch_up_should_abort(&error) {
                return Err(error);
            }

            tracing::warn!(
                target: "whitenoise::foreground_catchup",
                "Foreground discovery subscription catch-up failed, continuing with accounts: {}",
                error
            );
        }

        let accounts = Account::all(&self.shared.database).await?;
        for account in &accounts {
            if let Err(error) = self.refresh_account_subscriptions(account).await {
                tracing::warn!(
                    target: "whitenoise::foreground_catchup",
                    account = %account.pubkey.to_hex(),
                    "Foreground account subscription catch-up failed: {}",
                    error
                );
            }
        }

        Ok(accounts)
    }

    fn foreground_catch_up_should_abort(error: &WhitenoiseError) -> bool {
        matches!(
            error,
            WhitenoiseError::Database(_)
                | WhitenoiseError::SqlxError(_)
                | WhitenoiseError::MdkSqliteStorage(_)
                | WhitenoiseError::Filesystem(_)
                | WhitenoiseError::Configuration(_)
        )
    }

    async fn replay_foreground_stream_snapshots(&self, accounts: &[Account]) {
        self.replay_chat_list_stream_snapshots(accounts).await;
        self.replay_message_stream_snapshots(accounts).await;
    }

    async fn replay_chat_list_stream_snapshots(&self, accounts: &[Account]) {
        let active_subscribers: HashSet<_> = self
            .shared
            .chat_list_stream_manager
            .subscriber_pubkeys()
            .into_iter()
            .collect();
        let archived_subscribers: HashSet<_> = self
            .shared
            .archived_chat_list_stream_manager
            .subscriber_pubkeys()
            .into_iter()
            .collect();

        if active_subscribers.is_empty() && archived_subscribers.is_empty() {
            return;
        }

        for account in accounts {
            if active_subscribers.contains(&account.pubkey) {
                match self.fetch_chat_list_snapshot(account).await {
                    Ok(items) => {
                        for item in items {
                            self.shared.chat_list_stream_manager.emit(
                                &account.pubkey,
                                ChatListUpdate {
                                    trigger: ChatListUpdateTrigger::SnapshotRefresh,
                                    item,
                                },
                            );
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "whitenoise::foreground_catchup",
                            account = %account.pubkey.to_hex(),
                            "Failed to replay active chat-list snapshot: {}",
                            error
                        );
                    }
                }
            }

            if archived_subscribers.contains(&account.pubkey) {
                match self.fetch_archived_chat_list_snapshot(account).await {
                    Ok(items) => {
                        for item in items {
                            self.shared.archived_chat_list_stream_manager.emit(
                                &account.pubkey,
                                ChatListUpdate {
                                    trigger: ChatListUpdateTrigger::SnapshotRefresh,
                                    item,
                                },
                            );
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "whitenoise::foreground_catchup",
                            account = %account.pubkey.to_hex(),
                            "Failed to replay archived chat-list snapshot: {}",
                            error
                        );
                    }
                }
            }
        }
    }

    async fn replay_message_stream_snapshots(&self, accounts: &[Account]) {
        for group_id in self.shared.message_stream_manager.subscribed_group_ids() {
            self.replay_message_stream_snapshot_for_group(accounts, &group_id)
                .await;
        }
    }

    async fn replay_message_stream_snapshot_for_group(
        &self,
        accounts: &[Account],
        group_id: &GroupId,
    ) {
        for account in accounts {
            match self
                .fetch_aggregated_messages_for_group(
                    &account.pubkey,
                    group_id,
                    &PaginationOptions::default(),
                    Some(FOREGROUND_MESSAGE_SNAPSHOT_LIMIT),
                )
                .await
            {
                Ok(messages) => {
                    for message in messages {
                        self.shared.message_stream_manager.emit(
                            group_id,
                            MessageUpdate {
                                trigger: UpdateTrigger::SnapshotRefresh,
                                message,
                            },
                        );
                    }
                    return;
                }
                Err(WhitenoiseError::AccountNotFound | WhitenoiseError::GroupNotFound) => {}
                Err(error) => {
                    tracing::warn!(
                        target: "whitenoise::foreground_catchup",
                        account = %account.pubkey.to_hex(),
                        group_id = %hex::encode(group_id.as_slice()),
                        "Failed to replay message snapshot: {}",
                        error
                    );
                }
            }
        }

        tracing::debug!(
            target: "whitenoise::foreground_catchup",
            group_id = %hex::encode(group_id.as_slice()),
            "Skipped message snapshot replay because no account session could read the group"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use mdk_core::prelude::GroupId;
    use nostr_sdk::prelude::*;

    use super::*;
    use crate::whitenoise::{
        aggregated_message::AggregatedMessage,
        message_aggregator::{ChatMessage, ReactionSummary},
        test_utils::{create_mock_whitenoise, create_nostr_group_config_data, insert_test_account},
    };

    fn test_message(seed: u8, author: PublicKey, created_at: Timestamp) -> ChatMessage {
        ChatMessage {
            id: format!("{:0>64x}", seed),
            author,
            content: format!("resume snapshot message {seed}"),
            created_at,
            tags: Tags::new(),
            is_reply: false,
            reply_to_id: None,
            is_deleted: false,
            content_tokens: whitenoise_markdown::Document::default(),
            reactions: ReactionSummary::default(),
            kind: 9,
            media_attachments: vec![],
            delivery_status: None,
        }
    }

    #[tokio::test]
    async fn resume_after_background_is_idempotent_with_no_accounts() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        whitenoise.resume_after_background().await.unwrap();
        whitenoise.resume_after_background().await.unwrap();
    }

    #[tokio::test]
    async fn resume_after_background_continues_when_one_account_cannot_refresh() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let good_account = whitenoise.create_identity().await.unwrap();
        let failing_pubkey = Keys::generate().public_key();
        insert_test_account(&whitenoise.shared.database, &failing_pubkey).await;

        whitenoise.resume_after_background().await.unwrap();

        assert!(
            whitenoise
                .is_account_subscriptions_operational(&good_account)
                .await
                .unwrap(),
            "a per-account refresh failure must not stop other accounts"
        );
    }

    #[tokio::test]
    async fn fetch_chat_list_snapshot_rereads_current_db_without_stream_provider() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();
        let mut config = create_nostr_group_config_data(vec![creator.pubkey]);
        config.name = "Foreground snapshot".to_string();

        let group = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member.pubkey], config, None)
            .await
            .unwrap();

        let snapshot = whitenoise.fetch_chat_list_snapshot(&creator).await.unwrap();

        assert!(
            snapshot
                .iter()
                .any(|item| item.mls_group_id == group.mls_group_id),
            "snapshot fetch should reread the current chat list without opening a stream"
        );
    }

    #[tokio::test]
    async fn resume_after_background_replays_chat_list_snapshot_to_active_subscribers() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();
        let mut config = create_nostr_group_config_data(vec![creator.pubkey]);
        config.name = "Foreground replay".to_string();

        let group = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member.pubkey], config, None)
            .await
            .unwrap();

        let mut subscription = whitenoise.subscribe_to_chat_list(&creator).await.unwrap();
        while subscription.updates.try_recv().is_ok() {}

        whitenoise.resume_after_background().await.unwrap();

        let update = tokio::time::timeout(Duration::from_secs(5), subscription.updates.recv())
            .await
            .expect("resume should replay a chat-list snapshot")
            .expect("chat-list stream should stay open");

        assert_eq!(update.trigger, ChatListUpdateTrigger::SnapshotRefresh);
        assert_eq!(update.item.mls_group_id, group.mls_group_id);
    }

    #[tokio::test]
    async fn resume_after_background_replays_message_snapshot_to_active_subscribers() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();
        let mut config = create_nostr_group_config_data(vec![creator.pubkey]);
        config.name = "Message replay".to_string();

        let group = whitenoise
            .require_session(&creator.pubkey)
            .unwrap()
            .groups()
            .create_group(vec![member.pubkey], config, None)
            .await
            .unwrap();
        let group_id: GroupId = group.mls_group_id.clone();

        let mut subscription = whitenoise
            .subscribe_to_group_messages(&creator.pubkey, &group_id, None)
            .await
            .unwrap();
        assert!(subscription.initial_messages.is_empty());

        let message = test_message(42, member.pubkey, Timestamp::from(1_800_000_042));
        let session = whitenoise.require_session(&creator.pubkey).unwrap();
        AggregatedMessage::insert_message(
            &message,
            &group_id,
            &creator.pubkey,
            &session.account_db.inner,
        )
        .await
        .unwrap();

        whitenoise.resume_after_background().await.unwrap();

        let update = tokio::time::timeout(Duration::from_secs(5), subscription.updates.recv())
            .await
            .expect("resume should replay a message snapshot")
            .expect("message stream should stay open");

        assert_eq!(update.trigger, UpdateTrigger::SnapshotRefresh);
        assert_eq!(update.message.id, message.id);
    }
}

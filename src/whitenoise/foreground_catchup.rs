use std::collections::HashSet;

use crate::perf_instrument;
use crate::whitenoise::{
    Whitenoise,
    accounts::Account,
    chat_list_streaming::{ChatListUpdate, ChatListUpdateTrigger},
    error::{Result, WhitenoiseError},
};

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
    /// - Active chat-list streams in this process receive a replay of the
    ///   current DB snapshot where the existing stream type can carry it.
    ///
    /// What this does not guarantee:
    /// - Broadcasts emitted by the NSE process are not delivered into the Runner
    ///   process; this method rereads the shared DB instead.
    /// - Chat-list item streams cannot represent "the whole list is now exactly
    ///   this". Message streams are keyed by group only, while message snapshots
    ///   are account-specific, so this method does not replay message snapshots
    ///   onto active message streams. Call
    ///   [`Self::fetch_chat_list_snapshot`],
    ///   [`Self::fetch_archived_chat_list_snapshot`], and
    ///   [`Self::fetch_aggregated_messages_for_group`] after this method when a
    ///   Flutter view needs exact replacement.
    ///
    /// Returns an error only when the shared account list cannot be read or
    /// another process-wide prerequisite fails. Per-account catch-up failures
    /// are logged and skipped.
    #[perf_instrument("whitenoise")]
    pub async fn resume_after_background(&self) -> Result<()> {
        let accounts = self.force_foreground_subscription_catch_up().await?;
        self.replay_chat_list_stream_snapshots(&accounts).await;
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
            if let Err(error) = self.catch_up_account_subscriptions(account).await {
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
        // Relay/network/subscription errors are treated as transient catch-up
        // failures so foreground resume can still refresh other accounts and
        // replay local DB snapshots. Process-wide storage/config/init failures
        // mean the app cannot reliably read shared state, so return them.
        matches!(
            error,
            WhitenoiseError::Initialization
                | WhitenoiseError::Database(_)
                | WhitenoiseError::SqlxError(_)
                | WhitenoiseError::MdkSqliteStorage(_)
                | WhitenoiseError::Filesystem(_)
                | WhitenoiseError::Configuration(_)
        )
    }

    async fn replay_chat_list_stream_snapshots(&self, accounts: &[Account]) {
        // These are point-in-time subscriber snapshots used to avoid DB work
        // for accounts that had no active stream at resume time. A stream that
        // subscribes after this point receives its own initial snapshot from
        // subscribe_to_chat_list/subscribe_to_archived_chat_list, so it does not
        // depend on this replay.
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
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nostr_sdk::prelude::*;

    use super::*;
    use crate::whitenoise::test_utils::{
        create_mock_whitenoise, create_nostr_group_config_data, insert_test_account,
    };

    #[tokio::test]
    async fn resume_after_background_is_idempotent_with_no_accounts() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        whitenoise.resume_after_background().await.unwrap();
        whitenoise.resume_after_background().await.unwrap();
    }

    #[test]
    fn foreground_catch_up_abort_classification_separates_process_failures() {
        assert!(Whitenoise::foreground_catch_up_should_abort(
            &WhitenoiseError::Initialization
        ));
        assert!(Whitenoise::foreground_catch_up_should_abort(
            &WhitenoiseError::Configuration("bad config".to_string())
        ));
        assert!(!Whitenoise::foreground_catch_up_should_abort(
            &WhitenoiseError::AccountNotFound
        ));
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
}

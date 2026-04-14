use std::collections::HashSet;
use std::sync::Arc;

use nostr_sdk::prelude::*;

use crate::perf_instrument;
use crate::whitenoise::{
    Whitenoise,
    account_settings::AccountSettings,
    accounts::Account,
    accounts_groups::AccountGroup,
    database::{account_muted_users::AccountMutedUsers, processed_events::ProcessedEvent},
    error::{Result, WhitenoiseError},
    utils::timestamp_to_datetime,
};

impl Whitenoise {
    #[perf_instrument("event_handlers")]
    pub(crate) async fn handle_mute_list(&self, account: &Account, event: Event) -> Result<()> {
        let _permit = self.acquire_mute_list_guard(account).await?;
        let account_id = account.id.ok_or(WhitenoiseError::AccountNotFound)?;

        if event.pubkey != account.pubkey {
            tracing::debug!(
                target: "whitenoise::handle_mute_list",
                "Ignoring mute list not authored by account {}",
                account.pubkey.to_hex()
            );
            return Ok(());
        }

        if self.should_skip_mute_list(&event, account_id).await? {
            return Ok(());
        }

        let mut muted = crate::nostr_manager::utils::pubkeys_from_event(&event);
        muted.sort_unstable_by_key(|pk| pk.to_hex());
        muted.dedup();

        AccountMutedUsers::replace_all_for_account(&account.pubkey, &muted, &self.database).await?;

        if AccountSettings::refuse_invites_from_muted_users_for_pubkey(
            &account.pubkey,
            &self.database,
        )
        .await?
        {
            let muted_set: HashSet<PublicKey> = muted.iter().copied().collect();
            let pending = AccountGroup::pending_for_account(self, &account.pubkey).await?;
            for ag in pending {
                let Some(welcomer) = ag.welcomer_pubkey else {
                    continue;
                };
                if !muted_set.contains(&welcomer) {
                    continue;
                }
                // DB-only decline: push tokens are not shared until the user accepts the
                // invite, so `decline_account_group`'s relay publish for token removal is
                // unnecessary here and can block for a long time under test / flaky relays.
                if let Err(error) = ag.decline(self).await {
                    tracing::warn!(
                        target: "whitenoise::handle_mute_list",
                        account = %account.pubkey.to_hex(),
                        group = %hex::encode(ag.mls_group_id.as_slice()),
                        error = %error,
                        "Failed to auto-decline pending invite from muted welcomer"
                    );
                }
            }
        }

        self.event_tracker
            .track_processed_account_event(&event, &account.pubkey)
            .await?;

        tracing::debug!(
            target: "whitenoise::handle_mute_list",
            "Processed mute list: {} entries for {}",
            muted.len(),
            account.pubkey.to_hex()
        );

        Ok(())
    }

    #[perf_instrument("event_handlers")]
    async fn acquire_mute_list_guard(
        &self,
        account: &Account,
    ) -> Result<tokio::sync::OwnedSemaphorePermit> {
        let semaphore = self
            .mute_list_guards
            .entry(account.pubkey)
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1)))
            .clone();

        semaphore.acquire_owned().await.map_err(|_| {
            WhitenoiseError::MuteList("Failed to acquire mute list processing permit".to_string())
        })
    }

    #[perf_instrument("event_handlers")]
    async fn should_skip_mute_list(&self, event: &Event, account_id: i64) -> Result<bool> {
        if ProcessedEvent::exists(&event.id, Some(account_id), &self.database).await? {
            tracing::debug!(
                target: "whitenoise::handle_mute_list",
                "Skipping already processed event {}",
                event.id.to_hex()
            );
            return Ok(true);
        }

        if self.is_stale_mute_list(event, account_id).await? {
            return Ok(true);
        }

        Ok(false)
    }

    #[perf_instrument("event_handlers")]
    async fn is_stale_mute_list(&self, event: &Event, account_id: i64) -> Result<bool> {
        let event_time = timestamp_to_datetime(event.created_at)?;
        let newest_time =
            ProcessedEvent::newest_mute_list_timestamp(account_id, &self.database).await?;

        match newest_time {
            Some(newest) if event_time.timestamp_millis() <= newest.timestamp_millis() => {
                tracing::debug!(
                    target: "whitenoise::handle_mute_list",
                    "Ignoring stale mute list (event: {}, newest: {})",
                    event_time.timestamp_millis(),
                    newest.timestamp_millis()
                );
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use mdk_core::prelude::GroupId;
    use nostr_sdk::prelude::*;

    use crate::whitenoise::{
        account_settings::AccountSettings,
        accounts::{Account, AccountType},
        accounts_groups::AccountGroup,
        database::{account_muted_users::AccountMutedUsers, processed_events::ProcessedEvent},
        test_utils::*,
        utils::timestamp_to_datetime,
    };

    async fn build_mute_list_event(
        signer: &Keys,
        muted: &[PublicKey],
        timestamp: Option<Timestamp>,
    ) -> Event {
        let tags: Vec<Tag> = muted
            .iter()
            .map(|pk| Tag::custom(TagKind::p(), [pk.to_hex()]))
            .collect();

        let mut builder = EventBuilder::new(Kind::MuteList, "").tags(tags);
        if let Some(ts) = timestamp {
            builder = builder.custom_created_at(ts);
        }
        builder.sign(signer).await.unwrap()
    }

    #[tokio::test]
    async fn test_handle_mute_list_success() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let keys = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        let m1 = Keys::generate().public_key();
        let m2 = Keys::generate().public_key();
        let ts = Timestamp::from(1700000000u64);
        let event = build_mute_list_event(&keys, &[m1, m2], Some(ts)).await;

        whitenoise
            .handle_mute_list(&account, event.clone())
            .await
            .unwrap();

        assert!(
            AccountMutedUsers::is_muted(&account.pubkey, &m1, &whitenoise.database)
                .await
                .unwrap()
        );
        assert!(
            AccountMutedUsers::is_muted(&account.pubkey, &m2, &whitenoise.database)
                .await
                .unwrap()
        );

        let account_id = account.id.unwrap();
        assert!(
            ProcessedEvent::exists(&event.id, Some(account_id), &whitenoise.database)
                .await
                .unwrap()
        );

        let newest = ProcessedEvent::newest_mute_list_timestamp(account_id, &whitenoise.database)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            newest.timestamp_millis(),
            timestamp_to_datetime(ts).unwrap().timestamp_millis()
        );
    }

    #[tokio::test]
    async fn test_handle_mute_list_stale_ignored() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let keys = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        let newer = Keys::generate().public_key();
        let older_only = Keys::generate().public_key();

        let current_ts = Timestamp::now();
        let older_ts = Timestamp::from(current_ts.as_secs() - 3600);

        whitenoise
            .handle_mute_list(
                &account,
                build_mute_list_event(&keys, &[newer], Some(current_ts)).await,
            )
            .await
            .unwrap();

        whitenoise
            .handle_mute_list(
                &account,
                build_mute_list_event(&keys, &[older_only], Some(older_ts)).await,
            )
            .await
            .unwrap();

        assert!(
            AccountMutedUsers::is_muted(&account.pubkey, &newer, &whitenoise.database)
                .await
                .unwrap()
        );
        assert!(
            !AccountMutedUsers::is_muted(&account.pubkey, &older_only, &whitenoise.database)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_handle_mute_list_newer_replaces() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let keys = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        let first = Keys::generate().public_key();
        let second = Keys::generate().public_key();
        let t1 = Timestamp::from(Timestamp::now().as_secs() - 3600);
        let t2 = Timestamp::now();

        whitenoise
            .handle_mute_list(
                &account,
                build_mute_list_event(&keys, &[first], Some(t1)).await,
            )
            .await
            .unwrap();

        whitenoise
            .handle_mute_list(
                &account,
                build_mute_list_event(&keys, &[second], Some(t2)).await,
            )
            .await
            .unwrap();

        assert!(
            !AccountMutedUsers::is_muted(&account.pubkey, &first, &whitenoise.database)
                .await
                .unwrap()
        );
        assert!(
            AccountMutedUsers::is_muted(&account.pubkey, &second, &whitenoise.database)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_handle_mute_list_empty_clears() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let keys = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        let m = Keys::generate().public_key();
        let t1 = Timestamp::from(Timestamp::now().as_secs() - 100);
        let t2 = Timestamp::now();

        whitenoise
            .handle_mute_list(&account, build_mute_list_event(&keys, &[m], Some(t1)).await)
            .await
            .unwrap();
        whitenoise
            .handle_mute_list(&account, build_mute_list_event(&keys, &[], Some(t2)).await)
            .await
            .unwrap();

        assert!(
            !AccountMutedUsers::is_muted(&account.pubkey, &m, &whitenoise.database)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_handle_mute_list_wrong_author_ignored() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let other = Keys::generate();
        let m = Keys::generate().public_key();
        let event = build_mute_list_event(&other, &[m], None).await;

        whitenoise.handle_mute_list(&account, event).await.unwrap();

        assert!(
            !AccountMutedUsers::is_muted(&account.pubkey, &m, &whitenoise.database)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_handle_mute_list_requires_saved_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys = Keys::generate();
        let unsaved_account = Account {
            id: None,
            pubkey: keys.public_key(),
            user_id: 0,
            account_type: AccountType::Local,
            last_synced_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        let event = build_mute_list_event(&keys, &[Keys::generate().public_key()], None).await;

        assert!(
            whitenoise
                .handle_mute_list(&unsaved_account, event)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_handle_mute_list_declines_pending_invites_for_muted_welcomer() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let keys = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        let welcomer = Keys::generate().public_key();
        let group_id = GroupId::from_slice(&[7u8; 32]);
        let pending = AccountGroup {
            id: None,
            account_pubkey: account.pubkey,
            mls_group_id: group_id,
            user_confirmation: None,
            welcomer_pubkey: Some(welcomer),
            last_read_message_id: None,
            pin_order: None,
            dm_peer_pubkey: None,
            archived_at: None,
            removed_at: None,
            self_removed: false,
            muted_until: None,
            chat_cleared_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        pending.save(&whitenoise.database).await.unwrap();
        let mls_group_id = pending.mls_group_id;

        let ts = Timestamp::now();
        whitenoise
            .handle_mute_list(
                &account,
                build_mute_list_event(&keys, &[welcomer], Some(ts)).await,
            )
            .await
            .unwrap();

        let ag = AccountGroup::get(&whitenoise, &account.pubkey, &mls_group_id)
            .await
            .unwrap()
            .expect("account group");
        assert!(ag.is_declined());
    }

    #[tokio::test]
    async fn test_handle_mute_list_skips_decline_when_refuse_invites_disabled() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let keys = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();

        AccountSettings::update_refuse_invites_from_muted_users(
            &account.pubkey,
            false,
            &whitenoise.database,
        )
        .await
        .unwrap();

        let welcomer = Keys::generate().public_key();
        let group_id = GroupId::from_slice(&[8u8; 32]);
        let pending = AccountGroup {
            id: None,
            account_pubkey: account.pubkey,
            mls_group_id: group_id,
            user_confirmation: None,
            welcomer_pubkey: Some(welcomer),
            last_read_message_id: None,
            pin_order: None,
            dm_peer_pubkey: None,
            archived_at: None,
            removed_at: None,
            self_removed: false,
            muted_until: None,
            chat_cleared_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        pending.save(&whitenoise.database).await.unwrap();
        let mls_group_id = pending.mls_group_id;

        let ts = Timestamp::now();
        whitenoise
            .handle_mute_list(
                &account,
                build_mute_list_event(&keys, &[welcomer], Some(ts)).await,
            )
            .await
            .unwrap();

        let ag = AccountGroup::get(&whitenoise, &account.pubkey, &mls_group_id)
            .await
            .unwrap()
            .expect("account group");
        assert!(ag.is_pending());
    }
}

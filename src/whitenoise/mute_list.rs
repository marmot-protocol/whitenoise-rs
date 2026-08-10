#[cfg(test)]
mod tests {
    use chrono::Utc;
    use nostr_sdk::{EventBuilder, Keys, Kind, NostrSigner, Tag};

    use crate::whitenoise::database::mute_list::MuteListEntry;
    use crate::whitenoise::session::MuteListOps;
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    #[tokio::test]
    async fn parse_mute_list_entries_public_tags_only() {
        let keys = Keys::generate();
        let signer = keys.clone();
        let target = Keys::generate().public_key();

        let event = EventBuilder::new(Kind::MuteList, "")
            .tags([Tag::public_key(target)])
            .sign(&signer)
            .await
            .unwrap();

        let entries =
            MuteListOps::parse_mute_list_entries(&signer, &keys.public_key(), &event).await;

        let entries = entries.expect("should return Some");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, target);
        assert!(!entries[0].1); // is_private = false
    }

    #[tokio::test]
    async fn parse_mute_list_entries_empty_event_returns_empty_list() {
        let keys = Keys::generate();
        let signer = keys.clone();

        let event = EventBuilder::new(Kind::MuteList, "")
            .sign(&signer)
            .await
            .unwrap();

        let entries =
            MuteListOps::parse_mute_list_entries(&signer, &keys.public_key(), &event).await;

        let entries = entries.expect("should return Some");
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn parse_mute_list_entries_bad_content_returns_none() {
        let keys = Keys::generate();
        let signer = keys.clone();

        // Non-empty content that is not valid NIP-44 ciphertext
        let event = EventBuilder::new(Kind::MuteList, "not-valid-ciphertext")
            .sign(&signer)
            .await
            .unwrap();

        let entries =
            MuteListOps::parse_mute_list_entries(&signer, &keys.public_key(), &event).await;

        // Decrypt failure → None, so cache must not be replaced
        assert!(entries.is_none());
    }

    #[tokio::test]
    async fn block_user_returns_ok_if_already_blocked() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let target = Keys::generate().public_key();
        let session = whitenoise.require_session(&account.pubkey).unwrap();

        // Insert directly — bypasses relay so the test is Docker-independent.
        MuteListEntry::insert(&target, true, Utc::now(), &session.account_db)
            .await
            .unwrap();

        // block_user fast-path: exists() check fires before any sync.
        let result = session.mute_list().block_user(&target).await;
        assert!(
            result.is_ok(),
            "block_user on already-blocked user should be a no-op"
        );

        // Entry must still be there (not double-inserted or removed).
        assert!(
            MuteListEntry::exists(&target, &session.account_db)
                .await
                .unwrap()
        );
    }

    /// Self-block guard: `block_user(self.pubkey)` must return `Ok(())`
    /// without inserting a row (`session/mute_list.rs:48-50`). Without the
    /// guard, the publish path would put the account's own pubkey in its own
    /// NIP-51 mute list and the block emit would fire against the account
    /// itself — both observable in the UI as nonsense state.
    #[tokio::test]
    async fn block_user_self_block_is_no_op() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let session = whitenoise.require_session(&account.pubkey).unwrap();

        let result = session.mute_list().block_user(&account.pubkey).await;
        assert!(result.is_ok(), "block_user on self must return Ok");

        assert!(
            !MuteListEntry::exists(&account.pubkey, &session.account_db)
                .await
                .unwrap(),
            "self-block must not insert a mute-list entry"
        );
    }

    #[tokio::test]
    async fn unblock_user_returns_ok_if_not_blocked() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let target = Keys::generate().public_key();
        let session = whitenoise.require_session(&account.pubkey).unwrap();

        // Target is not blocked — unblock_user must return Ok without touching relays.
        let result = session.mute_list().unblock_user(&target).await;
        assert!(
            result.is_ok(),
            "unblock_user on non-blocked user should be a no-op"
        );
    }

    // ── get_blocked_users / is_user_blocked (pure DB, no relay) ─────────────

    #[tokio::test]
    async fn get_blocked_users_returns_all_blocked_for_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let target1 = Keys::generate().public_key();
        let target2 = Keys::generate().public_key();
        let session = whitenoise.require_session(&account.pubkey).unwrap();

        MuteListEntry::insert(&target1, true, Utc::now(), &session.account_db)
            .await
            .unwrap();
        MuteListEntry::insert(&target2, false, Utc::now(), &session.account_db)
            .await
            .unwrap();

        let blocked = session.mute_list().get_blocked_users().await.unwrap();
        assert_eq!(blocked.len(), 2);

        let pubkeys: Vec<_> = blocked.iter().map(|e| e.muted_pubkey).collect();
        assert!(pubkeys.contains(&target1));
        assert!(pubkeys.contains(&target2));
    }

    #[tokio::test]
    async fn is_user_blocked_returns_true_when_blocked() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let blocked_target = Keys::generate().public_key();
        let other_target = Keys::generate().public_key();
        let session = whitenoise.require_session(&account.pubkey).unwrap();

        MuteListEntry::insert(&blocked_target, true, Utc::now(), &session.account_db)
            .await
            .unwrap();

        assert!(
            session
                .mute_list()
                .is_user_blocked(&blocked_target)
                .await
                .unwrap(),
            "inserted target should be reported as blocked"
        );
        assert!(
            !session
                .mute_list()
                .is_user_blocked(&other_target)
                .await
                .unwrap(),
            "uninserted target should not be reported as blocked"
        );
    }

    #[tokio::test]
    async fn get_blocked_users_returns_empty_for_account_with_no_blocks() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let session = whitenoise.require_session(&account.pubkey).unwrap();

        let blocked = session.mute_list().get_blocked_users().await.unwrap();
        assert!(blocked.is_empty());
    }

    // ── parse_mute_list_entries: private tags ────────────────────────────────

    #[tokio::test]
    async fn parse_mute_list_entries_private_tags_decrypted() {
        let keys = Keys::generate();
        let signer = keys.clone();
        let target = Keys::generate().public_key();

        // Build the encrypted private content: [["p", "<hex>"]]
        let private_json =
            serde_json::to_string(&vec![vec!["p".to_string(), target.to_hex()]]).unwrap();
        let encrypted = signer
            .nip44_encrypt(&keys.public_key(), &private_json)
            .await
            .unwrap();

        let event = EventBuilder::new(Kind::MuteList, encrypted)
            .sign(&signer)
            .await
            .unwrap();

        let entries =
            MuteListOps::parse_mute_list_entries(&signer, &keys.public_key(), &event).await;

        let entries = entries.expect("should return Some");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, target);
        assert!(entries[0].1); // is_private = true
    }

    #[tokio::test]
    async fn parse_mute_list_entries_private_and_public_combined() {
        let keys = Keys::generate();
        let signer = keys.clone();
        let public_target = Keys::generate().public_key();
        let private_target = Keys::generate().public_key();

        let private_json =
            serde_json::to_string(&vec![vec!["p".to_string(), private_target.to_hex()]]).unwrap();
        let encrypted = signer
            .nip44_encrypt(&keys.public_key(), &private_json)
            .await
            .unwrap();

        let event = EventBuilder::new(Kind::MuteList, encrypted)
            .tags([Tag::public_key(public_target)])
            .sign(&signer)
            .await
            .unwrap();

        let entries =
            MuteListOps::parse_mute_list_entries(&signer, &keys.public_key(), &event).await;

        let entries = entries.expect("should return Some");
        assert_eq!(entries.len(), 2);

        let public_entry = entries.iter().find(|(pk, _)| *pk == public_target);
        let private_entry = entries.iter().find(|(pk, _)| *pk == private_target);
        assert!(public_entry.is_some() && !public_entry.unwrap().1);
        assert!(private_entry.is_some() && private_entry.unwrap().1);
    }

    #[tokio::test]
    async fn parse_mute_list_entries_invalid_json_content_returns_none() {
        let keys = Keys::generate();
        let signer = keys.clone();

        // Encrypt valid ciphertext but the decrypted content is not valid tag JSON
        let invalid_json = "not-a-json-array";
        let encrypted = signer
            .nip44_encrypt(&keys.public_key(), invalid_json)
            .await
            .unwrap();

        let event = EventBuilder::new(Kind::MuteList, encrypted)
            .sign(&signer)
            .await
            .unwrap();

        let entries =
            MuteListOps::parse_mute_list_entries(&signer, &keys.public_key(), &event).await;

        // JSON parse failure → None
        assert!(entries.is_none());
    }

    // ── sync_and_emit ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn sync_and_emit_updates_cache_from_empty() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let target1 = Keys::generate().public_key();
        let target2 = Keys::generate().public_key();

        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let entries = vec![(target1, true), (target2, false)];
        session
            .mute_list()
            .sync_and_emit(&entries, Utc::now())
            .await
            .unwrap();

        let blocked = session.mute_list().get_blocked_users().await.unwrap();
        assert_eq!(blocked.len(), 2);
        assert!(
            MuteListEntry::exists(&target1, &session.account_db)
                .await
                .unwrap()
        );
        assert!(
            MuteListEntry::exists(&target2, &session.account_db)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn sync_and_emit_replaces_old_entries() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let old_target = Keys::generate().public_key();
        let new_target = Keys::generate().public_key();
        let session = whitenoise.require_session(&account.pubkey).unwrap();

        // Seed with old_target
        MuteListEntry::insert(&old_target, true, Utc::now(), &session.account_db)
            .await
            .unwrap();

        // Sync with only new_target
        session
            .mute_list()
            .sync_and_emit(&[(new_target, false)], Utc::now())
            .await
            .unwrap();

        assert!(
            !MuteListEntry::exists(&old_target, &session.account_db)
                .await
                .unwrap(),
            "old_target should be removed"
        );
        assert!(
            MuteListEntry::exists(&new_target, &session.account_db)
                .await
                .unwrap(),
            "new_target should be present"
        );
    }

    #[tokio::test]
    async fn sync_and_emit_with_empty_entries_clears_cache() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let target = Keys::generate().public_key();

        let session = whitenoise.require_session(&account.pubkey).unwrap();
        MuteListEntry::insert(&target, true, Utc::now(), &session.account_db)
            .await
            .unwrap();

        session
            .mute_list()
            .sync_and_emit(&[], Utc::now())
            .await
            .unwrap();

        let blocked = session.mute_list().get_blocked_users().await.unwrap();
        assert!(blocked.is_empty());
    }

    // ── emit_block_changed: no DM group ──────────────────────────────────────

    #[tokio::test]
    async fn emit_block_changed_noop_when_no_dm_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let stranger = Keys::generate().public_key();

        // No DM group exists for this pair — emit_block_changed must not panic.
        // We verify indirectly: block_user path calls it only after publish,
        // so we insert directly and call sync_and_emit which internally calls it.
        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let entries = vec![(stranger, true)];
        let result = session
            .mute_list()
            .sync_and_emit(&entries, Utc::now())
            .await;
        assert!(
            result.is_ok(),
            "sync_and_emit with no DM group must succeed"
        );
    }
}

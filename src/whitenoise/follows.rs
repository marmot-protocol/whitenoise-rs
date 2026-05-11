#[cfg(test)]
mod tests {
    use nostr_sdk::Keys;

    use crate::whitenoise::test_utils::*;

    #[tokio::test]
    async fn test_new_account_has_no_follows() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let session = whitenoise.require_session(&account.pubkey).unwrap();

        let follows = session.social().follows().await.unwrap();

        assert!(follows.is_empty());
    }

    #[tokio::test]
    async fn test_follow_multiple_users() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let session = whitenoise.require_session(&account.pubkey).unwrap();

        let target1 = Keys::generate().public_key();
        let target2 = Keys::generate().public_key();
        let target3 = Keys::generate().public_key();

        session.social().follow_user(&target1).await.unwrap();
        session.social().follow_user(&target2).await.unwrap();
        session.social().follow_user(&target3).await.unwrap();

        let follows = session.social().follows().await.unwrap();
        assert_eq!(follows.len(), 3);

        let pubkeys: Vec<_> = follows.iter().map(|u| u.pubkey).collect();
        assert!(pubkeys.contains(&target1));
        assert!(pubkeys.contains(&target2));
        assert!(pubkeys.contains(&target3));
    }

    #[tokio::test]
    async fn test_follow_is_idempotent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let target_pubkey = Keys::generate().public_key();
        let session = whitenoise.require_session(&account.pubkey).unwrap();

        session.social().follow_user(&target_pubkey).await.unwrap();
        session.social().follow_user(&target_pubkey).await.unwrap();

        let follows = session.social().follows().await.unwrap();
        assert_eq!(follows.len(), 1);
    }

    #[tokio::test]
    async fn test_unfollow_preserves_other_follows() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let session = whitenoise.require_session(&account.pubkey).unwrap();

        let target1 = Keys::generate().public_key();
        let target2 = Keys::generate().public_key();
        let target3 = Keys::generate().public_key();

        session.social().follow_user(&target1).await.unwrap();
        session.social().follow_user(&target2).await.unwrap();
        session.social().follow_user(&target3).await.unwrap();

        session.social().unfollow_user(&target2).await.unwrap();

        let follows = session.social().follows().await.unwrap();
        assert_eq!(follows.len(), 2);

        let pubkeys: Vec<_> = follows.iter().map(|u| u.pubkey).collect();
        assert!(pubkeys.contains(&target1));
        assert!(!pubkeys.contains(&target2));
        assert!(pubkeys.contains(&target3));
    }

    #[tokio::test]
    async fn test_unfollow_nonexistent_user_succeeds() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let target_pubkey = Keys::generate().public_key();
        let session = whitenoise.require_session(&account.pubkey).unwrap();

        assert!(
            whitenoise
                .find_user_by_pubkey(&target_pubkey)
                .await
                .is_err()
        );

        let result = session.social().unfollow_user(&target_pubkey).await;

        assert!(result.is_ok());
        // Unfollowing should not create a user record as a side effect
        assert!(
            whitenoise
                .find_user_by_pubkey(&target_pubkey)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_is_following_returns_false_for_nonexistent_user() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let nonexistent_pubkey = Keys::generate().public_key();
        let session = whitenoise.require_session(&account.pubkey).unwrap();

        let is_following = session
            .social()
            .is_following(&nonexistent_pubkey)
            .await
            .unwrap();

        assert!(!is_following);
    }

    #[tokio::test]
    async fn test_follow_creates_user_record() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let new_user_pubkey = Keys::generate().public_key();
        let session = whitenoise.require_session(&account.pubkey).unwrap();

        assert!(
            whitenoise
                .find_user_by_pubkey(&new_user_pubkey)
                .await
                .is_err()
        );

        session
            .social()
            .follow_user(&new_user_pubkey)
            .await
            .unwrap();

        let user = whitenoise.find_user_by_pubkey(&new_user_pubkey).await;
        assert!(user.is_ok());
        assert_eq!(user.unwrap().pubkey, new_user_pubkey);
    }

    #[tokio::test]
    async fn test_follow_unfollow_refollow_lifecycle() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let target_pubkey = Keys::generate().public_key();
        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let social = session.social();

        // Initially not following
        assert!(!social.is_following(&target_pubkey).await.unwrap());

        // Follow
        social.follow_user(&target_pubkey).await.unwrap();
        assert!(social.is_following(&target_pubkey).await.unwrap());
        assert_eq!(social.follows().await.unwrap().len(), 1);

        // Unfollow
        social.unfollow_user(&target_pubkey).await.unwrap();
        assert!(!social.is_following(&target_pubkey).await.unwrap());
        assert!(social.follows().await.unwrap().is_empty());

        // Refollow
        social.follow_user(&target_pubkey).await.unwrap();
        assert!(social.is_following(&target_pubkey).await.unwrap());

        let follows = social.follows().await.unwrap();
        assert_eq!(follows.len(), 1);
        assert_eq!(follows[0].pubkey, target_pubkey);
    }

    #[tokio::test]
    async fn test_account_follow_isolation() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account1 = whitenoise.create_identity().await.unwrap();
        let account2 = whitenoise.create_identity().await.unwrap();
        let session1 = whitenoise.require_session(&account1.pubkey).unwrap();
        let session2 = whitenoise.require_session(&account2.pubkey).unwrap();

        let exclusive_target = Keys::generate().public_key();
        let shared_target = Keys::generate().public_key();

        // Account1 follows exclusive target, both follow shared target
        session1
            .social()
            .follow_user(&exclusive_target)
            .await
            .unwrap();
        session1.social().follow_user(&shared_target).await.unwrap();
        session2.social().follow_user(&shared_target).await.unwrap();

        // Verify isolation: account1 has 2 follows, account2 has 1
        let account1_follows = session1.social().follows().await.unwrap();
        let account2_follows = session2.social().follows().await.unwrap();
        assert_eq!(account1_follows.len(), 2);
        assert_eq!(account2_follows.len(), 1);

        // Account2 shouldn't see account1's exclusive target
        assert!(
            !session2
                .social()
                .is_following(&exclusive_target)
                .await
                .unwrap()
        );

        // Unfollowing shared target from account1 shouldn't affect account2
        session1
            .social()
            .unfollow_user(&shared_target)
            .await
            .unwrap();
        assert!(
            !session1
                .social()
                .is_following(&shared_target)
                .await
                .unwrap()
        );
        assert!(
            session2
                .social()
                .is_following(&shared_target)
                .await
                .unwrap()
        );
    }
}

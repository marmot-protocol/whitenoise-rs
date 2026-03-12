use nostr_sdk::PublicKey;

use crate::perf_instrument;
use crate::whitenoise::{
    Whitenoise,
    accounts::Account,
    error::{Result, WhitenoiseError},
    users::User,
};

impl Whitenoise {
    /// Creates a follow relationship between an account and a user.
    ///
    /// This method establishes a follow relationship by creating an entry in the `account_follows`
    /// table, linking the account to the user. The user is looked up by their public key, and the
    /// relationship is timestamped with the current time for both creation and update timestamps.
    ///
    /// # Arguments
    ///
    /// * `account` - The account that will follow the user (must exist in database with valid ID)
    /// * `pubkey` - The public key of the user to be followed
    #[perf_instrument("follows")]
    pub async fn follow_user(&self, account: &Account, pubkey: &PublicKey) -> Result<()> {
        let (user, newly_created) = User::find_or_create_by_pubkey(pubkey, &self.database).await?;

        if newly_created {
            self.background_fetch_user_data(&user).await?;
        }

        account.follow_user(&user, &self.database).await?;
        self.background_publish_account_follow_list(account).await?;
        Ok(())
    }

    /// Removes a follow relationship between an account and a user.
    ///
    /// This method removes an existing follow relationship by deleting the corresponding
    /// entry from the `account_follows` table. The user is looked up by their public key.
    /// If no relationship exists, the operation succeeds without error.
    ///
    /// # Arguments
    ///
    /// * `account` - The account that will unfollow the user (must exist in database with valid ID)
    /// * `pubkey` - The public key of the user to be unfollowed
    #[perf_instrument("follows")]
    pub async fn unfollow_user(&self, account: &Account, pubkey: &PublicKey) -> Result<()> {
        let user = match self.find_user_by_pubkey(pubkey).await {
            Ok(user) => user,
            Err(WhitenoiseError::UserNotFound) => return Ok(()),
            Err(e) => return Err(e),
        };
        account.unfollow_user(&user, &self.database).await?;
        self.background_publish_account_follow_list(account).await?;
        Ok(())
    }

    /// Checks if an account is following a specific user.
    ///
    /// This method queries the `account_follows` table to determine whether a follow
    /// relationship exists between the specified account and user. The user is looked
    /// up by their public key.
    ///
    /// # Arguments
    ///
    /// * `account` - The account to check (must exist in database with valid ID)
    /// * `pubkey` - The public key of the user to check if followed
    #[perf_instrument("follows")]
    pub async fn is_following_user(&self, account: &Account, pubkey: &PublicKey) -> Result<bool> {
        let user = match self.find_user_by_pubkey(pubkey).await {
            Ok(user) => user,
            Err(WhitenoiseError::UserNotFound) => return Ok(false),
            Err(e) => return Err(e),
        };
        account.is_following_user(&user, &self.database).await
    }

    /// Retrieves all users that an account follows.
    ///
    /// This method queries the `account_follows` table to get a complete list of users
    /// that the specified account is following. The returned users include their full
    /// metadata and profile information.
    ///
    /// # Arguments
    ///
    /// * `account` - The account whose follows to retrieve (must exist in database with valid ID)
    #[perf_instrument("follows")]
    pub async fn follows(&self, account: &Account) -> Result<Vec<User>> {
        account.follows(&self.database).await
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::Keys;

    use crate::whitenoise::test_utils::*;

    #[tokio::test]
    async fn test_new_account_has_no_follows() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let follows = whitenoise.follows(&account).await.unwrap();

        assert!(follows.is_empty());
    }

    #[tokio::test]
    async fn test_follow_multiple_users() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let target1 = Keys::generate().public_key();
        let target2 = Keys::generate().public_key();
        let target3 = Keys::generate().public_key();

        whitenoise.follow_user(&account, &target1).await.unwrap();
        whitenoise.follow_user(&account, &target2).await.unwrap();
        whitenoise.follow_user(&account, &target3).await.unwrap();

        let follows = whitenoise.follows(&account).await.unwrap();
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

        whitenoise
            .follow_user(&account, &target_pubkey)
            .await
            .unwrap();
        whitenoise
            .follow_user(&account, &target_pubkey)
            .await
            .unwrap();

        let follows = whitenoise.follows(&account).await.unwrap();
        assert_eq!(follows.len(), 1);
    }

    #[tokio::test]
    async fn test_unfollow_preserves_other_follows() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let target1 = Keys::generate().public_key();
        let target2 = Keys::generate().public_key();
        let target3 = Keys::generate().public_key();

        whitenoise.follow_user(&account, &target1).await.unwrap();
        whitenoise.follow_user(&account, &target2).await.unwrap();
        whitenoise.follow_user(&account, &target3).await.unwrap();

        whitenoise.unfollow_user(&account, &target2).await.unwrap();

        let follows = whitenoise.follows(&account).await.unwrap();
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

        assert!(
            whitenoise
                .find_user_by_pubkey(&target_pubkey)
                .await
                .is_err()
        );

        let result = whitenoise.unfollow_user(&account, &target_pubkey).await;

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

        let is_following = whitenoise
            .is_following_user(&account, &nonexistent_pubkey)
            .await
            .unwrap();

        assert!(!is_following);
    }

    #[tokio::test]
    async fn test_follow_creates_user_record() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let new_user_pubkey = Keys::generate().public_key();

        assert!(
            whitenoise
                .find_user_by_pubkey(&new_user_pubkey)
                .await
                .is_err()
        );

        whitenoise
            .follow_user(&account, &new_user_pubkey)
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

        // Initially not following
        assert!(
            !whitenoise
                .is_following_user(&account, &target_pubkey)
                .await
                .unwrap()
        );

        // Follow
        whitenoise
            .follow_user(&account, &target_pubkey)
            .await
            .unwrap();
        assert!(
            whitenoise
                .is_following_user(&account, &target_pubkey)
                .await
                .unwrap()
        );
        assert_eq!(whitenoise.follows(&account).await.unwrap().len(), 1);

        // Unfollow
        whitenoise
            .unfollow_user(&account, &target_pubkey)
            .await
            .unwrap();
        assert!(
            !whitenoise
                .is_following_user(&account, &target_pubkey)
                .await
                .unwrap()
        );
        assert!(whitenoise.follows(&account).await.unwrap().is_empty());

        // Refollow
        whitenoise
            .follow_user(&account, &target_pubkey)
            .await
            .unwrap();
        assert!(
            whitenoise
                .is_following_user(&account, &target_pubkey)
                .await
                .unwrap()
        );

        let follows = whitenoise.follows(&account).await.unwrap();
        assert_eq!(follows.len(), 1);
        assert_eq!(follows[0].pubkey, target_pubkey);
    }

    #[tokio::test]
    async fn test_account_follow_isolation() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account1 = whitenoise.create_identity().await.unwrap();
        let account2 = whitenoise.create_identity().await.unwrap();

        let exclusive_target = Keys::generate().public_key();
        let shared_target = Keys::generate().public_key();

        // Account1 follows exclusive target, both follow shared target
        whitenoise
            .follow_user(&account1, &exclusive_target)
            .await
            .unwrap();
        whitenoise
            .follow_user(&account1, &shared_target)
            .await
            .unwrap();
        whitenoise
            .follow_user(&account2, &shared_target)
            .await
            .unwrap();

        // Verify isolation: account1 has 2 follows, account2 has 1
        let account1_follows = whitenoise.follows(&account1).await.unwrap();
        let account2_follows = whitenoise.follows(&account2).await.unwrap();
        assert_eq!(account1_follows.len(), 2);
        assert_eq!(account2_follows.len(), 1);

        // Account2 shouldn't see account1's exclusive target
        assert!(
            !whitenoise
                .is_following_user(&account2, &exclusive_target)
                .await
                .unwrap()
        );

        // Unfollowing shared target from account1 shouldn't affect account2
        whitenoise
            .unfollow_user(&account1, &shared_target)
            .await
            .unwrap();
        assert!(
            !whitenoise
                .is_following_user(&account1, &shared_target)
                .await
                .unwrap()
        );
        assert!(
            whitenoise
                .is_following_user(&account2, &shared_target)
                .await
                .unwrap()
        );
    }
}

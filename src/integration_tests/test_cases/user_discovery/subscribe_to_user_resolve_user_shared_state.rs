use async_trait::async_trait;
use nostr_sdk::Keys;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;

pub struct SubscribeToUserResolveUserSharedStateTestCase {
    test_keys: Keys,
}

impl SubscribeToUserResolveUserSharedStateTestCase {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for SubscribeToUserResolveUserSharedStateTestCase {
    fn default() -> Self {
        Self {
            test_keys: Keys::generate(),
        }
    }
}

#[async_trait]
impl TestCase for SubscribeToUserResolveUserSharedStateTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let test_pubkey = self.test_keys.public_key();

        let resolved_user = context.whitenoise.resolve_user(&test_pubkey).await?;
        let subscription = context.whitenoise.subscribe_to_user(&test_pubkey).await?;
        let stored_user = context.whitenoise.find_user_by_pubkey(&test_pubkey).await?;

        assert_eq!(resolved_user.pubkey, test_pubkey);
        assert!(resolved_user.metadata_is_unknown());
        assert_eq!(stored_user, subscription.initial_user);
        assert_eq!(stored_user.pubkey, resolved_user.pubkey);
        assert_eq!(stored_user.id, resolved_user.id);
        assert_eq!(
            stored_user.metadata_known_at,
            resolved_user.metadata_known_at
        );

        tracing::info!(
            "✓ subscribe_to_user and resolve_user share the same initial local snapshot semantics"
        );

        Ok(())
    }
}

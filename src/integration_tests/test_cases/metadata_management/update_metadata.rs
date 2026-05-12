use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use nostr_sdk::Metadata;

pub struct UpdateMetadataTestCase {
    account_name: String,
    metadata: Metadata,
}

impl UpdateMetadataTestCase {
    pub fn for_account(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            metadata: Metadata::default(),
        }
    }

    pub fn with_name(mut self, name: &str) -> Self {
        self.metadata.name = Some(name.to_string());
        self.metadata.display_name = Some(name.to_string());
        self
    }

    pub fn with_about(mut self, about: &str) -> Self {
        self.metadata.about = Some(about.to_string());
        self
    }

    pub fn with_picture(mut self, picture: &str) -> Self {
        self.metadata.picture = Some(picture.to_string());
        self
    }

    pub fn with_website(mut self, website: &str) -> Self {
        self.metadata.website = Some(website.to_string());
        self
    }

    pub fn with_nip05(mut self, nip05: &str) -> Self {
        self.metadata.nip05 = Some(nip05.to_string());
        self
    }
}

#[async_trait]
impl TestCase for UpdateMetadataTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!("Updating metadata for account: {}", self.account_name);

        let account = context.get_account(&self.account_name)?;
        account
            .update_metadata(&self.metadata, &context.whitenoise)
            .await?;

        // Verify the update worked
        let updated_metadata = account.metadata(&context.whitenoise).await?;
        assert_eq!(
            updated_metadata.name, self.metadata.name,
            "Name was not updated correctly"
        );
        assert_eq!(
            updated_metadata.display_name, self.metadata.display_name,
            "Display name was not updated correctly"
        );
        assert_eq!(
            updated_metadata.about, self.metadata.about,
            "About was not updated correctly"
        );
        assert_eq!(
            updated_metadata.picture, self.metadata.picture,
            "Picture was not updated correctly"
        );
        assert_eq!(
            updated_metadata.website, self.metadata.website,
            "Website was not updated correctly"
        );
        assert_eq!(
            updated_metadata.nip05, self.metadata.nip05,
            "Nip05 was not updated correctly"
        );

        tracing::info!(
            "✓ Metadata updated and verified for {}: {:?}",
            self.account_name,
            updated_metadata.name
        );
        Ok(())
    }
}

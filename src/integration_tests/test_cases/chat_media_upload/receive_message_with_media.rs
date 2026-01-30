use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use nostr_sdk::prelude::*;

/// Test case for receiving messages with media attachments and verifying MediaFile records are created
///
/// This test verifies that when a user receives an MLS message containing imeta tags (MIP-04),
/// the system automatically extracts and stores media references with:
/// - original_file_hash (from imeta 'x' field)
/// - encrypted_file_hash (from Blossom URL)
/// - empty file_path (not downloaded yet)
/// - full metadata from imeta tags
pub struct ReceiveMessageWithMediaTestCase {
    sender_account_name: String,
    receiver_account_name: String,
    group_name: String,
    message_content: String,
}

impl ReceiveMessageWithMediaTestCase {
    pub fn new(sender_account_name: &str, receiver_account_name: &str, group_name: &str) -> Self {
        Self {
            sender_account_name: sender_account_name.to_string(),
            receiver_account_name: receiver_account_name.to_string(),
            group_name: group_name.to_string(),
            message_content: "Check out this cool image! 🖼️".to_string(),
        }
    }

    /// Build imeta tag per MIP-04 spec
    /// Format: `["imeta", "url <blossom_url>", "m <mime_type>", "filename <name>", "x <hash>", "n <nonce>", "v <version>"]`
    /// Note: MIP-04 requires url, m, filename, x, n, and v fields
    fn build_imeta_tag(
        &self,
        original_hash_hex: &str,
        blossom_url: &str,
        mime_type: &str,
        filename: Option<&str>,
        nonce_hex: &str,
    ) -> Result<Tag, WhitenoiseError> {
        let parts = vec![
            "imeta".to_string(),
            format!("url {}", blossom_url),
            format!("m {}", mime_type),
            format!("filename {}", filename.unwrap_or("image.jpg")),
            format!("x {}", original_hash_hex),
            format!("n {}", nonce_hex),
            format!("v mip04-v2"),
        ];

        Tag::parse(parts).map_err(|e| {
            WhitenoiseError::Other(anyhow::anyhow!("Failed to create imeta tag: {}", e))
        })
    }
}

#[async_trait]
impl TestCase for ReceiveMessageWithMediaTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Testing receive message with media: sender={}, receiver={}, group={}",
            self.sender_account_name,
            self.receiver_account_name,
            self.group_name
        );

        let sender_account = context.get_account(&self.sender_account_name)?;
        let receiver_account = context.get_account(&self.receiver_account_name)?;
        let group = context.get_group(&self.group_name)?;

        // Get the uploaded media file from context (uploaded earlier in scenario)
        let media_file = context.get_media_file("uploaded_chat_media")?;

        // MIP-04: imeta 'x' field must contain original_file_hash
        let original_hash = media_file.original_file_hash.as_ref().ok_or_else(|| {
            WhitenoiseError::Configuration(
                "Chat media must have original_file_hash for MIP-04".to_string(),
            )
        })?;
        let original_hash_hex = hex::encode(original_hash);

        let blossom_url = media_file.blossom_url.as_ref().ok_or_else(|| {
            WhitenoiseError::Configuration("Uploaded media has no blossom URL".to_string())
        })?;

        let filename = media_file
            .file_metadata
            .as_ref()
            .and_then(|meta| meta.original_filename.as_deref());

        // Get nonce from media_file (required for MIP-04 v2)
        let nonce_hex = media_file.nonce.as_ref().ok_or_else(|| {
            WhitenoiseError::Configuration(
                "Chat media must have nonce for MIP-04 v2".to_string(),
            )
        })?;

        let imeta_tag = self.build_imeta_tag(
            &original_hash_hex,
            blossom_url,
            &media_file.mime_type,
            filename,
            nonce_hex,
        )?;

        tracing::info!("✓ Built imeta tag with original_hash={}", original_hash_hex);

        // Send message with imeta tag from sender
        let send_result = context
            .whitenoise
            .send_message_to_group(
                sender_account,
                &group.mls_group_id,
                self.message_content.clone(),
                9, // Regular message
                Some(vec![imeta_tag.clone()]),
            )
            .await?;

        tracing::info!(
            "✓ Message with media reference sent: {}",
            send_result.message.id
        );

        // Wait for message processing and event handlers to complete
        // The handle_mls_message handler should extract and store media references
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        // Query MediaFile records for this group
        // This should find the media reference created by store_references_from_imeta_tags
        let receiver_media_files = retry(
            15,
            std::time::Duration::from_millis(100),
            || async {
                let files = context
                    .whitenoise
                    .get_media_files_for_group(&group.mls_group_id)
                    .await?;

                // Filter to media files for receiver account with matching original_file_hash
                let matching: Vec<_> = files
                    .into_iter()
                    .filter(|mf| {
                        // Check it's the receiver's media file
                        mf.account_pubkey == receiver_account.pubkey
                            // Check it matches our sent media
                            && mf.original_file_hash
                                .as_ref()
                                .map(|hash| hex::encode(hash) == original_hash_hex)
                                .unwrap_or(false)
                    })
                    .collect();

                if matching.is_empty() {
                    return Err(WhitenoiseError::Other(anyhow::anyhow!(
                        "Media reference not yet created for receiver"
                    )));
                }

                Ok(matching)
            },
            "find media reference on receiver's database",
        )
        .await?;

        assert!(
            !receiver_media_files.is_empty(),
            "Receiver should have MediaFile record created from imeta tag"
        );

        let receiver_media = &receiver_media_files[0];

        // Verify original_file_hash is populated (from imeta 'x' field)
        let receiver_original_hash =
            receiver_media.original_file_hash.as_ref().ok_or_else(|| {
                WhitenoiseError::Other(anyhow::anyhow!(
                    "Receiver's MediaFile should have original_file_hash from imeta 'x' field"
                ))
            })?;

        assert_eq!(
            hex::encode(receiver_original_hash),
            original_hash_hex,
            "Receiver's original_file_hash should match imeta 'x' field (MIP-04)"
        );

        tracing::info!(
            "✓ Receiver's MediaFile has correct original_file_hash: {}",
            hex::encode(receiver_original_hash)
        );

        // Verify encrypted_file_hash is populated (from Blossom URL)
        assert!(
            !receiver_media.encrypted_file_hash.is_empty(),
            "Receiver's MediaFile should have encrypted_file_hash from Blossom URL"
        );

        tracing::info!(
            "✓ Receiver's MediaFile has encrypted_file_hash: {}",
            hex::encode(&receiver_media.encrypted_file_hash)
        );

        // Verify file_path is empty (not downloaded yet)
        assert!(
            receiver_media.file_path.to_str().unwrap_or("").is_empty(),
            "Receiver's MediaFile should have empty file_path (not downloaded yet)"
        );

        tracing::info!("✓ Receiver's MediaFile has empty file_path (not downloaded)");

        // Verify metadata was extracted correctly
        assert_eq!(
            receiver_media.mime_type, media_file.mime_type,
            "MIME type should match imeta 'm' field"
        );

        assert_eq!(
            receiver_media.media_type, "chat_media",
            "media_type should be 'chat_media'"
        );

        assert_eq!(
            receiver_media.blossom_url.as_ref().unwrap(),
            blossom_url,
            "Blossom URL should match imeta 'url' field"
        );

        tracing::info!("✓ Receiver's MediaFile metadata matches imeta tags");

        // Verify nostr_key is None (chat media uses MDK, not key/nonce encryption)
        assert!(
            receiver_media.nostr_key.is_none(),
            "chat_media should not have nostr_key (uses MDK encryption)"
        );

        tracing::info!(
            "✓ Media reference successfully created on receiver's database with both hashes"
        );
        tracing::info!(
            "  • original_file_hash: {} (from imeta 'x' field)",
            hex::encode(receiver_original_hash)
        );
        tracing::info!(
            "  • encrypted_file_hash: {} (from Blossom URL)",
            hex::encode(&receiver_media.encrypted_file_hash)
        );
        tracing::info!("  • file_path: empty (not downloaded)");
        tracing::info!("  • media_type: {}", receiver_media.media_type);

        Ok(())
    }
}

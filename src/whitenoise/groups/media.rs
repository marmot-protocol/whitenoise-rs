use mdk_core::prelude::GroupId;
use nostr_blossom::client::BlossomClient;
use nostr_sdk::prelude::*;

use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::error::Result;

impl Whitenoise {
    /// Spawns a background task to sync group image cache without blocking
    ///
    /// This is used by event handlers to proactively cache group images
    /// without blocking event processing. Failures are logged but don't
    /// affect the caller - images will download on-demand if needed.
    ///
    /// # Arguments
    /// * `account` - The account viewing the group
    /// * `group_id` - The MLS group ID
    pub(crate) fn background_sync_group_image_cache_if_needed(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) {
        let Some(session) = self.session(&account.pubkey) else {
            tracing::error!(
                target: "whitenoise::groups::background_sync_group_image_cache_if_needed",
                account = %account.pubkey,
                "No active session for background image cache"
            );
            return;
        };
        let group_id = group_id.clone();
        tokio::spawn(async move {
            if let Err(e) = session
                .groups()
                .media()
                .sync_group_image_cache_if_needed(&group_id)
                .await
            {
                tracing::warn!(
                    target: "whitenoise::groups::background_sync_group_image_cache_if_needed",
                    "Background image cache failed: {}. Image will download on-demand.",
                    e
                );
            }
        });
    }
}

/// Blossom client that enforces HTTPS on the server URL.
pub(crate) fn blossom_client(url: &Url) -> Result<BlossomClient> {
    require_https(url)?;
    Ok(BlossomClient::new(url.clone()))
}

pub(crate) fn is_debug_local_blossom_url(url: &Url) -> bool {
    if !cfg!(debug_assertions) {
        return false;
    }

    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    }
}

/// Rejects non-HTTPS Blossom URLs to prevent cleartext metadata leakage.
/// Debug builds also allow loopback `http://` URLs for local testing.
pub(crate) fn require_https(url: &Url) -> Result<()> {
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_debug_local_blossom_url(url) => Ok(()),
        _ => Err(crate::whitenoise::error::WhitenoiseError::BlossomInsecureUrl(url.to_string())),
    }
}

use std::path::PathBuf;
use std::time::Duration;

#[cfg(test)]
use std::sync::LazyLock;

#[cfg(test)]
use futures::StreamExt;
use mdk_core::media_processing::MediaProcessingOptions;
use mdk_core::prelude::{GroupId, group_types};
use nostr_blossom::client::BlossomClient;
use nostr_sdk::prelude::*;

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::database::media_files::MediaFile;
use crate::whitenoise::error::{Result, WhitenoiseError};

/// Shared HTTP client for Blossom blob downloads.
///
/// `reqwest::Client` holds a connection pool, a DNS resolver, and TLS state.
/// Building it once here lets all downloads reuse keep-alive connections and
/// avoids repeated TLS handshakes.  The ring TLS provider is installed here
/// too, so the `install_default()` call happens exactly once.
#[cfg(test)]
static BLOSSOM_HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    // reqwest 0.13 with `rustls-no-provider` requires an explicit provider.
    // The `let _ =` suppresses the error when it is already installed (e.g.
    // during tests where multiple call sites race to install the same provider).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Use embedded Mozilla root certificates instead of rustls-platform-verifier.
    // The platform verifier requires Android-specific JNI initialization that is
    // not performed in the Flutter app, causing a panic on first HTTPS request.
    // Embedded roots work on all platforms and match nostr-blossom's approach.
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    // Custom redirect policy that mirrors `require_https()`:
    // - Always allow HTTPS redirect targets.
    // - In debug builds, also allow HTTP loopback hosts (for local test servers).
    // - Reject everything else to prevent HTTPS → HTTP downgrade attacks.
    // - Cap the chain at 10 hops to guard against redirect loops.
    let redirect_policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        let url = attempt.url();
        let allowed = match url.scheme() {
            "https" => true,
            "http" if Whitenoise::is_debug_local_blossom_url(url) => true,
            _ => false,
        };
        if allowed {
            attempt.follow()
        } else {
            let msg = format!("redirect to non-HTTPS URL is not allowed: {url}");
            attempt.error(msg)
        }
    });

    reqwest::Client::builder()
        .redirect(redirect_policy)
        .use_preconfigured_tls(tls_config)
        .build()
        .expect("Failed to build shared Blossom HTTP client")
});

impl Whitenoise {
    /// Default timeout for Blossom HTTP operations (download and upload)
    /// Set to 300 seconds to accommodate large image files over slow connections
    pub(crate) const BLOSSOM_TIMEOUT: Duration = Duration::from_secs(300);

    /// Maximum number of bytes accepted for a single blob download (100 MiB).
    ///
    /// This cap applies both to the `Content-Length` header check (early rejection)
    /// and to the streaming byte counter (guards against servers that lie about or
    /// omit the header).  A 100 MiB limit is generous for encrypted media files
    /// while still bounding worst-case memory pressure.
    #[cfg(test)]
    const MAX_BLOB_BYTES: usize = 100 * 1024 * 1024; // 100 MiB

    /// Syncs group image cache if needed (smart, hash-based check)
    ///
    /// This method is called after processing welcomes and commits to proactively
    /// cache group images. It only downloads if:
    /// 1. The group has an image set
    /// 2. The image_hash is not already cached
    ///
    /// This ensures images are ready before the UI needs them, while avoiding
    /// redundant downloads.
    ///
    /// # Arguments
    /// * `account` - The account viewing the group
    /// * `group_id` - The MLS group ID
    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().media().sync_group_image_cache_if_needed() instead."
    )]
    #[perf_instrument("media")]
    pub(crate) async fn sync_group_image_cache_if_needed(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<()> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session
            .groups()
            .media()
            .sync_group_image_cache_if_needed(group_id)
            .await
    }

    /// Spawns a background task to sync group image cache without blocking
    ///
    /// This is used by event handlers to proactively cache group images
    /// without blocking event processing. Failures are logged but don't
    /// affect the caller - images will download on-demand if needed.
    ///
    /// # Arguments
    /// * `account` - The account viewing the group
    /// * `group_id` - The MLS group ID
    #[allow(deprecated)]
    pub(crate) fn background_sync_group_image_cache_if_needed(
        account: &Account,
        group_id: &GroupId,
    ) {
        let account_clone = account.clone();
        let group_id_clone = group_id.clone();
        tokio::spawn(async move {
            let whitenoise = match Whitenoise::get_instance() {
                Ok(wn) => wn,
                Err(e) => {
                    tracing::error!(
                        target: "whitenoise::groups::background_sync_group_image_cache_if_needed",
                        "Failed to get Whitenoise instance for background image cache: {}",
                        e
                    );
                    return;
                }
            };

            if let Err(e) = whitenoise
                .sync_group_image_cache_if_needed(&account_clone, &group_id_clone)
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

    /// Uploads a group image to a Blossom server and returns the encrypted metadata.
    ///
    /// The returned metadata (hash, key, nonce) should be passed to `update_group_data`
    /// to update the group's image settings.
    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().media().upload_group_image() instead."
    )]
    #[perf_instrument("media")]
    pub async fn upload_group_image(
        &self,
        account: &Account,
        group_id: &GroupId,
        file_path: &str,
        blossom_server_url: Option<Url>,
        options: Option<MediaProcessingOptions>,
    ) -> Result<([u8; 32], [u8; 32], [u8; 12])> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session
            .groups()
            .media()
            .upload_group_image(group_id, file_path, blossom_server_url, options)
            .await
    }

    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().media().upload_chat_media() instead."
    )]
    #[perf_instrument("media")]
    pub async fn upload_chat_media(
        &self,
        account: &Account,
        group_id: &GroupId,
        file_path: &str,
        blossom_server_url: Option<Url>,
        options: Option<MediaProcessingOptions>,
    ) -> Result<MediaFile> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session
            .groups()
            .media()
            .upload_chat_media(group_id, file_path, blossom_server_url, options)
            .await
    }

    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().media().download_chat_media() instead."
    )]
    #[perf_instrument("media")]
    pub async fn download_chat_media(
        &self,
        account: &Account,
        group_id: &GroupId,
        original_file_hash: &[u8; 32],
    ) -> Result<MediaFile> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session
            .groups()
            .media()
            .download_chat_media(group_id, original_file_hash)
            .await
    }

    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().media().get_media_files_for_group() instead."
    )]
    #[perf_instrument("media")]
    pub async fn get_media_files_for_group(&self, group_id: &GroupId) -> Result<Vec<MediaFile>> {
        // NOTE: Cannot delegate through session.groups().media() — this wrapper has no `account`
        // parameter and therefore no session to look up. Both paths query the same DB table;
        // this divergence is harmless until the method signature is updated in phase 15+.
        MediaFile::find_by_group(&self.database, group_id).await
    }

    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().media().get_group_image_path() instead."
    )]
    #[perf_instrument("media")]
    pub async fn get_group_image_path(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<Option<PathBuf>> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session
            .groups()
            .media()
            .get_group_image_path(group_id)
            .await
    }

    #[deprecated(
        since = "0.0.0",
        note = "Use AccountSession::groups().media().resolve_group_image_path() instead."
    )]
    #[perf_instrument("media")]
    pub(crate) async fn resolve_group_image_path(
        &self,
        account: &Account,
        group: &group_types::Group,
    ) -> Result<Option<PathBuf>> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session
            .groups()
            .media()
            .resolve_group_image_path(group)
            .await
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
            "http" if Self::is_debug_local_blossom_url(url) => Ok(()),
            _ => Err(WhitenoiseError::BlossomInsecureUrl(url.to_string())),
        }
    }

    /// Blossom client that enforces HTTPS on the server URL.
    pub(crate) fn blossom_client(url: &Url) -> Result<BlossomClient> {
        Self::require_https(url)?;
        Ok(BlossomClient::new(url.clone()))
    }

    /// Validates a `Content-Length` header value against `limit`.
    ///
    /// Returns the length as `usize` (suitable for `Vec::with_capacity`) on
    /// success, or `DownloadSizeLimitExceeded` if the advertised length exceeds
    /// the limit.  The `u64 → usize` conversion uses `try_from` with a
    /// `usize::MAX` fallback to stay correct on any target width.
    #[cfg(test)]
    fn check_content_length(content_length: u64, limit: usize) -> Result<usize> {
        let content_length = usize::try_from(content_length).unwrap_or(usize::MAX);
        if content_length > limit {
            return Err(WhitenoiseError::DownloadSizeLimitExceeded { limit });
        }
        Ok(content_length)
    }

    #[cfg(test)]
    #[perf_instrument("media")]
    async fn download_blob_from_blossom(
        blossom_url: &Url,
        image_hash: &[u8; 32],
    ) -> Result<Vec<u8>> {
        // Enforce HTTPS before making any network contact.
        Self::require_https(blossom_url)?;

        // Build the Blossom download URL: <base>/<hex-hash>
        // nostr-blossom's get_blob does this the same way, but we need the raw
        // reqwest Response so we can stream it with a byte cap instead of
        // buffering the whole body unconditionally.
        let hash_hex = hex::encode(image_hash);
        let download_url = blossom_url.join(&hash_hex).map_err(|e| {
            WhitenoiseError::BlossomDownload(format!("Failed to build download URL: {}", e))
        })?;

        tokio::time::timeout(Self::BLOSSOM_TIMEOUT, async {
            let response = BLOSSOM_HTTP_CLIENT
                .get(download_url)
                .send()
                .await
                .map_err(|e| {
                    WhitenoiseError::BlossomDownload(format!("HTTP request failed: {}", e))
                })?;

            if !response.status().is_success() {
                return Err(WhitenoiseError::BlossomDownload(format!(
                    "Server returned status {}",
                    response.status()
                )));
            }

            // Early rejection: refuse downloads that advertise an oversized body.
            // Content-Length can be omitted or spoofed, so this is a fast-path
            // check only — the streaming counter below is the authoritative guard.
            let hint_capacity = match response.content_length() {
                Some(cl) => Self::check_content_length(cl, Self::MAX_BLOB_BYTES)?,
                None => 0,
            };

            // Stream the body chunk-by-chunk, counting bytes as they arrive.
            // This is the authoritative size guard: it fires even when the server
            // omits or lies about Content-Length.
            //
            // The subtraction `MAX_BLOB_BYTES - body.len()` is always safe: the
            // loop only continues while body.len() <= MAX_BLOB_BYTES, so the
            // right-hand side never underflows.
            let mut body: Vec<u8> = Vec::with_capacity(hint_capacity);
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| {
                    WhitenoiseError::BlossomDownload(format!("Error reading response body: {}", e))
                })?;
                if chunk.len() > Self::MAX_BLOB_BYTES - body.len() {
                    return Err(WhitenoiseError::DownloadSizeLimitExceeded {
                        limit: Self::MAX_BLOB_BYTES,
                    });
                }
                body.extend_from_slice(&chunk);
            }

            Ok(body)
        })
        .await
        .map_err(|_| {
            WhitenoiseError::BlossomDownload(format!(
                "Download timed out after {} seconds",
                Self::BLOSSOM_TIMEOUT.as_secs()
            ))
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── download_blob_from_blossom size-cap tests ─────────────────────────────
    //
    // These tests spin up a local mockito server and call the real streaming
    // path.  They run only in debug builds because `require_https` allows
    // loopback `http://` URLs in debug mode only.

    /// Returns a fixed 32-byte hash for use as a fake blob hash in tests.
    fn test_hash() -> [u8; 32] {
        [0xab; 32]
    }

    /// Returns the URL path that `download_blob_from_blossom` will request for
    /// `test_hash()`, i.e. `/<hex-encoded-hash>`.
    fn test_path() -> String {
        format!("/{}", hex::encode(test_hash()))
    }

    /// Returns a `localhost`-based URL for the mockito server.
    ///
    /// `mockito::Server` binds to a loopback address; using `localhost` keeps the
    /// test URL readable while still reaching the local mock server.
    fn localhost_url(server: &mockito::Server) -> Url {
        let port = server.socket_address().port();
        Url::parse(&format!("http://localhost:{port}")).unwrap()
    }

    /// Fast-path rejection: `Content-Length` header alone exceeds the limit.
    ///
    /// `check_content_length` is the pure function behind the fast-path guard.
    /// Testing it directly avoids the need for a mock HTTP server and verifies
    /// the logic independently of the network layer.
    #[test]
    fn content_length_guard_rejects_oversized_header() {
        let limit = Whitenoise::MAX_BLOB_BYTES;
        let oversized = (limit + 1) as u64;
        let err = Whitenoise::check_content_length(oversized, limit).unwrap_err();
        assert!(
            matches!(err, WhitenoiseError::DownloadSizeLimitExceeded { limit: l } if l == Whitenoise::MAX_BLOB_BYTES),
            "Expected DownloadSizeLimitExceeded, got: {err:?}"
        );
    }

    /// Fast-path accepts a `Content-Length` exactly at the limit.
    #[test]
    fn content_length_guard_accepts_exact_limit() {
        let limit = Whitenoise::MAX_BLOB_BYTES;
        let result = Whitenoise::check_content_length(limit as u64, limit).unwrap();
        assert_eq!(result, limit);
    }

    /// Fast-path accepts a `Content-Length` below the limit.
    #[test]
    fn content_length_guard_accepts_small_value() {
        let result = Whitenoise::check_content_length(1024, Whitenoise::MAX_BLOB_BYTES).unwrap();
        assert_eq!(result, 1024);
    }

    /// Authoritative guard: server streams > 100 MiB with no `Content-Length`.
    ///
    /// The streaming byte counter must catch this even when the header is absent.
    /// `with_chunked_body` is used (not `with_body`) because mockito's `with_body`
    /// automatically sets `Content-Length`, which would trigger the fast-path header
    /// check instead of exercising the streaming byte counter.
    #[tokio::test]
    #[cfg(debug_assertions)]
    async fn download_rejects_oversized_streaming_body() {
        let mut server = mockito::Server::new_async().await;
        // One byte over the limit, sent as a chunked response without Content-Length.
        let oversized_body = vec![0u8; Whitenoise::MAX_BLOB_BYTES + 1];
        let _mock = server
            .mock("GET", test_path().as_str())
            .with_status(200)
            .with_chunked_body(move |w| w.write_all(&oversized_body))
            .create_async()
            .await;

        let url = localhost_url(&server);
        let err = Whitenoise::download_blob_from_blossom(&url, &test_hash())
            .await
            .unwrap_err();

        assert!(
            matches!(
                err,
                WhitenoiseError::DownloadSizeLimitExceeded {
                    limit: Whitenoise::MAX_BLOB_BYTES
                }
            ),
            "Expected DownloadSizeLimitExceeded, got: {err:?}"
        );
    }

    /// Happy path: a small response within the limit is returned successfully.
    #[tokio::test]
    #[cfg(debug_assertions)]
    async fn download_accepts_small_body() {
        let mut server = mockito::Server::new_async().await;
        let body = b"hello blossom";
        let _mock = server
            .mock("GET", test_path().as_str())
            .with_status(200)
            .with_body(&body[..])
            .create_async()
            .await;

        let url = localhost_url(&server);
        let result = Whitenoise::download_blob_from_blossom(&url, &test_hash())
            .await
            .unwrap();

        assert_eq!(result, body);
    }

    // ── require_https / blossom_client tests ─────────────────────────────────

    #[test]
    fn blossom_client_accepts_https() {
        let url = Url::parse("https://blossom.primal.net").unwrap();
        assert!(Whitenoise::blossom_client(&url).is_ok());
    }

    #[test]
    fn blossom_client_rejects_http() {
        let url = Url::parse("http://evil.example.com").unwrap();
        let err = Whitenoise::blossom_client(&url).unwrap_err();
        assert!(
            matches!(err, WhitenoiseError::BlossomInsecureUrl(_)),
            "Expected BlossomInsecureUrl, got: {err:?}"
        );
    }

    #[test]
    fn blossom_client_rejects_ftp() {
        let url = Url::parse("ftp://files.example.com/blob").unwrap();
        assert!(Whitenoise::blossom_client(&url).is_err());
    }

    #[test]
    #[cfg(debug_assertions)]
    fn blossom_client_allows_localhost_http_in_debug() {
        let url = Url::parse("http://localhost:3000").unwrap();
        assert!(Whitenoise::blossom_client(&url).is_ok());
    }

    #[test]
    #[cfg(debug_assertions)]
    fn blossom_client_allows_loopback_http_in_debug() {
        let url = Url::parse("http://127.0.0.1:3000").unwrap();
        assert!(Whitenoise::blossom_client(&url).is_ok());
    }

    #[test]
    #[cfg(debug_assertions)]
    fn blossom_client_rejects_non_localhost_http_in_debug() {
        let url = Url::parse("http://192.168.1.1:3000").unwrap();
        assert!(Whitenoise::blossom_client(&url).is_err());
    }
}

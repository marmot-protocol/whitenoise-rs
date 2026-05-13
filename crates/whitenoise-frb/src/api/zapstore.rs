use crate::api::error::ApiError;
use flutter_rust_bridge::frb;

/// Fetches the latest version string published on Zapstore for White Noise.
///
/// Returns `None` when no release has been published yet or the relay is unreachable.
#[frb]
pub async fn fetch_latest_zapstore_version() -> Result<Option<String>, ApiError> {
    whitenoise::fetch_latest_zapstore_version()
        .await
        .map_err(ApiError::from)
}

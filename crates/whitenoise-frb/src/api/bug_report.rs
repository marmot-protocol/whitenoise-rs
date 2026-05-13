use std::time::Duration;

use crate::api::{DEFAULT_RELAY_URLS, error::ApiError};
use flutter_rust_bridge::frb;
use nostr_sdk::prelude::*;
use serde_json::{Value, json};
use tracing::warn;

const BUG_REPORT_EVENT_KIND: u16 = 0xDEAD;

const BUG_REPORT_RECIPIENT_PUBKEY: &str =
    "960d8044d4b0f3981bf512a231b6e7c687e9e1199ec3e4df3179746118ec9f08";

const MAX_LOG_BYTES: usize = 64 * 1024;

#[frb]
#[allow(clippy::too_many_arguments)]
pub async fn send_bug_report(
    what_went_wrong: String,
    expected_behavior: Option<String>,
    steps_to_reproduce: Option<String>,
    frequency: Option<String>,
    npub: Option<String>,
    logs: Option<String>,
    app_version: String,
    platform: String,
    os_version: String,
) -> Result<(), ApiError> {
    let ephemeral_keys = Keys::generate();

    let recipient = PublicKey::parse(BUG_REPORT_RECIPIENT_PUBKEY).map_err(|e| ApiError::Other {
        message: e.to_string(),
    })?;

    let mut report: Value = json!({
        "app_version": app_version,
        "platform": platform,
        "os_version": os_version,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "what_went_wrong": what_went_wrong,
    });
    let obj = report.as_object_mut().ok_or_else(|| ApiError::Other {
        message: "Failed to build report object".to_string(),
    })?;
    if let Some(v) = expected_behavior {
        obj.insert("expected_behavior".into(), json!(v));
    }
    if let Some(v) = steps_to_reproduce {
        obj.insert("steps_to_reproduce".into(), json!(v));
    }
    if let Some(v) = frequency {
        obj.insert("frequency".into(), json!(v));
    }
    if let Some(v) = npub {
        obj.insert("npub".into(), json!(v));
    }
    if let Some(v) = logs {
        let truncated = if v.len() > MAX_LOG_BYTES {
            let boundary = (0..=MAX_LOG_BYTES)
                .rev()
                .find(|&i| v.is_char_boundary(i))
                .unwrap_or(0);
            v[..boundary].to_string()
        } else {
            v
        };
        obj.insert("logs".into(), json!(truncated));
    }

    let content = serde_json::to_string(&report).map_err(|e| ApiError::Other {
        message: e.to_string(),
    })?;

    let encrypted = nip44::encrypt(
        ephemeral_keys.secret_key(),
        &recipient,
        &content,
        nip44::Version::V2,
    )
    .map_err(|e| ApiError::Other {
        message: e.to_string(),
    })?;

    let event = EventBuilder::new(Kind::Custom(BUG_REPORT_EVENT_KIND), encrypted)
        .tag(Tag::public_key(recipient))
        .sign_with_keys(&ephemeral_keys)
        .map_err(|e| ApiError::Other {
            message: e.to_string(),
        })?;

    let client = Client::new(ephemeral_keys);
    for url in DEFAULT_RELAY_URLS {
        if let Err(e) = client.add_relay(url).await {
            warn!(%url, error = %e, "Failed to add relay for bug report");
        }
    }

    let connect_output = client.try_connect(Duration::from_secs(10)).await;
    if connect_output.success.is_empty() {
        client.shutdown().await;
        return Err(ApiError::Other {
            message: "Could not connect to any relay".to_string(),
        });
    }

    let output = client
        .send_event(&event)
        .await
        .map_err(|e| ApiError::Other {
            message: e.to_string(),
        })?;

    client.shutdown().await;

    if output.success.is_empty() {
        return Err(ApiError::Other {
            message: "Bug report could not be delivered to any relay".to_string(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::DEFAULT_RELAY_URLS;

    #[test]
    fn test_recipient_pubkey_parses_and_event_builds() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let recipient =
                PublicKey::parse(BUG_REPORT_RECIPIENT_PUBKEY).expect("hardcoded pubkey must parse");

            let ephemeral_keys = Keys::generate();
            let content = r#"{"test":"smoke"}"#;

            let encrypted = nip44::encrypt(
                ephemeral_keys.secret_key(),
                &recipient,
                content,
                nip44::Version::V2,
            )
            .expect("nip44 encrypt must succeed");

            let event = EventBuilder::new(Kind::Custom(BUG_REPORT_EVENT_KIND), encrypted)
                .tag(Tag::public_key(recipient))
                .sign_with_keys(&ephemeral_keys)
                .expect("event signing must succeed");

            assert!(event.verify().is_ok(), "event signature must verify");
        });
    }

    #[test]
    fn test_log_truncation_at_max_bytes() {
        let big_log = "x".repeat(MAX_LOG_BYTES + 100);
        let mut report = json!({});
        let obj = report.as_object_mut().unwrap();
        let truncated = if big_log.len() > MAX_LOG_BYTES {
            let boundary = (0..=MAX_LOG_BYTES)
                .rev()
                .find(|&i| big_log.is_char_boundary(i))
                .unwrap_or(0);
            big_log[..boundary].to_string()
        } else {
            big_log.clone()
        };
        obj.insert("logs".into(), json!(truncated));
        let logs_val = report["logs"].as_str().unwrap();
        assert_eq!(logs_val.len(), MAX_LOG_BYTES);
    }

    #[test]
    fn test_log_truncation_is_char_boundary_safe() {
        // 3-byte UTF-8 char; MAX_LOG_BYTES may land mid-char
        let char_3byte = "あ"; // U+3042, 3 bytes
        let repeat_count = MAX_LOG_BYTES / 3 + 1;
        let big_log = char_3byte.repeat(repeat_count);
        assert!(big_log.len() > MAX_LOG_BYTES);
        let boundary = (0..=MAX_LOG_BYTES)
            .rev()
            .find(|&i| big_log.is_char_boundary(i))
            .unwrap_or(0);
        let truncated = big_log[..boundary].to_string();
        assert!(truncated.len() <= MAX_LOG_BYTES);
        assert!(big_log.is_char_boundary(truncated.len()));
    }

    #[test]
    fn test_bug_report_relays_match_defaults() {
        let expected = DEFAULT_RELAY_URLS
            .iter()
            .map(|url| (*url).to_string())
            .collect::<Vec<_>>();
        assert_eq!(expected, crate::api::default_relay_urls());
    }
}

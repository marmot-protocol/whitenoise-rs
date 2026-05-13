use crate::api::wn_session;
use chrono::{DateTime, Utc};
use flutter_rust_bridge::frb;
use nostr_sdk::PublicKey;
use whitenoise::MuteListEntry as WhitenoiseEntry;

use crate::api::ApiError;

#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct MuteListEntry {
    pub account_pubkey: String,
    pub muted_pubkey: String,
    pub is_private: bool,
    pub created_at: DateTime<Utc>,
}

impl MuteListEntry {
    fn from_entry(account_pubkey: &PublicKey, e: WhitenoiseEntry) -> Self {
        Self {
            account_pubkey: account_pubkey.to_hex(),
            muted_pubkey: e.muted_pubkey.to_hex(),
            is_private: e.is_private,
            created_at: e.created_at,
        }
    }
}

#[frb]
pub async fn block_user(account_pubkey: String, target_pubkey: String) -> Result<(), ApiError> {
    let account_pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&account_pubkey)?;
    let target = PublicKey::parse(&target_pubkey)?;
    session
        .mute_list()
        .block_user(&target)
        .await
        .map_err(ApiError::from)
}

#[frb]
pub async fn unblock_user(account_pubkey: String, target_pubkey: String) -> Result<(), ApiError> {
    let account_pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&account_pubkey)?;
    let target = PublicKey::parse(&target_pubkey)?;
    session
        .mute_list()
        .unblock_user(&target)
        .await
        .map_err(ApiError::from)
}

#[frb]
pub async fn get_blocked_users(account_pubkey: String) -> Result<Vec<MuteListEntry>, ApiError> {
    let account_pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&account_pubkey)?;
    let entries = session.mute_list().get_blocked_users().await?;
    Ok(entries
        .into_iter()
        .map(|e| MuteListEntry::from_entry(&account_pubkey, e))
        .collect())
}

#[frb]
pub async fn is_user_blocked(
    account_pubkey: String,
    target_pubkey: String,
) -> Result<bool, ApiError> {
    let account_pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&account_pubkey)?;
    let target = PublicKey::parse(&target_pubkey)?;
    session
        .mute_list()
        .is_user_blocked(&target)
        .await
        .map_err(ApiError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nostr_sdk::Keys;
    use whitenoise::MuteListEntry as WhitenoiseEntry;

    #[test]
    fn test_mute_list_entry_conversion() {
        let account_pubkey = Keys::generate().public_key();
        let muted_keys = Keys::generate();
        let now = Utc::now();

        let entry = WhitenoiseEntry {
            muted_pubkey: muted_keys.public_key(),
            is_private: true,
            created_at: now,
        };

        let flutter_entry = MuteListEntry::from_entry(&account_pubkey, entry);

        assert_eq!(flutter_entry.account_pubkey, account_pubkey.to_hex());
        assert_eq!(flutter_entry.muted_pubkey, muted_keys.public_key().to_hex());
        assert!(flutter_entry.is_private);
        assert_eq!(flutter_entry.created_at, now);
    }

    #[test]
    fn test_mute_list_entry_conversion_public() {
        let account_pubkey = Keys::generate().public_key();
        let muted_keys = Keys::generate();

        let entry = WhitenoiseEntry {
            muted_pubkey: muted_keys.public_key(),
            is_private: false,
            created_at: Utc::now(),
        };

        let flutter_entry = MuteListEntry::from_entry(&account_pubkey, entry);
        assert!(!flutter_entry.is_private);
    }
}

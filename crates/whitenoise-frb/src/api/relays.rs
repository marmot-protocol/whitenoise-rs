use crate::api::error::ApiError;
use crate::api::wn;
use chrono::{DateTime, Utc};
use flutter_rust_bridge::frb;
use nostr_sdk::prelude::*;
use whitenoise::{Relay as WhitenoiseRelay, RelayType};

#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct Relay {
    pub url: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<WhitenoiseRelay> for Relay {
    fn from(relay: WhitenoiseRelay) -> Self {
        Self {
            url: relay.url.to_string(),
            created_at: relay.created_at,
            updated_at: relay.updated_at,
        }
    }
}

#[frb]
pub fn relay_type_nip65() -> RelayType {
    RelayType::Nip65
}

#[frb]
pub fn relay_type_inbox() -> RelayType {
    RelayType::Inbox
}

#[frb]
pub fn relay_type_key_package() -> RelayType {
    RelayType::KeyPackage
}

#[frb]
pub async fn debug_relay_control_state() -> Result<String, ApiError> {
    let whitenoise = wn()?;
    whitenoise
        .debug_relay_control_state()
        .await
        .map_err(ApiError::from)
}

#[frb]
pub async fn ensure_all_subscriptions() -> Result<(), ApiError> {
    let whitenoise = wn()?;
    whitenoise
        .ensure_all_subscriptions()
        .await
        .map_err(ApiError::from)
}

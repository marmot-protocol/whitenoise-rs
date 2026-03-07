use nostr_sdk::{PublicKey, RelayUrl};

use super::utils::create_column_decode_error;
use crate::relay_control::{
    RelayPlane,
    observability::{RelayFailureCategory, RelayTelemetryKind},
};

pub(crate) fn parse_relay_url(value: String) -> Result<RelayUrl, sqlx::Error> {
    RelayUrl::parse(&value).map_err(|error| sqlx::Error::ColumnDecode {
        index: "relay_url".to_string(),
        source: Box::new(error),
    })
}

pub(crate) fn parse_relay_plane(value: String) -> Result<RelayPlane, sqlx::Error> {
    value
        .parse::<RelayPlane>()
        .map_err(|error| create_column_decode_error("plane", &error))
}

pub(crate) fn parse_telemetry_kind(value: String) -> Result<RelayTelemetryKind, sqlx::Error> {
    value
        .parse::<RelayTelemetryKind>()
        .map_err(|error| create_column_decode_error("telemetry_kind", &error))
}

pub(crate) fn parse_failure_category(value: String) -> Result<RelayFailureCategory, sqlx::Error> {
    value
        .parse::<RelayFailureCategory>()
        .map_err(|error| create_column_decode_error("failure_category", &error))
}

pub(crate) fn parse_optional_public_key(
    value: Option<String>,
) -> Result<Option<PublicKey>, sqlx::Error> {
    let Some(value) = value else {
        return Ok(None);
    };

    PublicKey::from_hex(&value)
        .map(Some)
        .map_err(|error| create_column_decode_error("account_pubkey", &error.to_string()))
}

pub(crate) fn serialize_optional_public_key(account_pubkey: Option<PublicKey>) -> Option<String> {
    account_pubkey.map(|pubkey| pubkey.to_hex())
}

//! Utility functions and data structures for White Noise API.
//!
//! This module provides essential utility functions for the White Noise Flutter application,
//! including key management, relay operations, and data conversions.

use crate::api::error::ApiError;
use crate::api::{Language, ThemeMode};
use flutter_rust_bridge::frb;
use nostr_sdk::prelude::*;
pub use whitenoise::Whitenoise;
use whitenoise::mdk::GroupId;

#[frb(sync)]
pub fn npub_from_hex_pubkey(hex_pubkey: &str) -> Result<String, ApiError> {
    Whitenoise::npub_from_hex_pubkey(hex_pubkey).map_err(ApiError::from)
}

#[frb(sync)]
pub fn hex_pubkey_from_npub(npub: &str) -> Result<String, ApiError> {
    let pubkey = PublicKey::parse(npub).map_err(ApiError::from)?;
    Ok(pubkey.to_hex())
}

#[frb]
pub fn relay_url_from_string(url: String) -> Result<RelayUrl, ApiError> {
    RelayUrl::parse(&url).map_err(ApiError::from)
}

#[frb]
pub fn string_from_relay_url(relay_url: &RelayUrl) -> String {
    relay_url.to_string()
}

#[frb]
pub fn tag_from_vec(vec: Vec<String>) -> Result<Tag, ApiError> {
    Ok(Tag::parse(&vec)?)
}

#[frb]
pub fn get_default_blossom_server_url() -> String {
    #[cfg(debug_assertions)]
    {
        "http://localhost:3000".to_string()
    }
    #[cfg(not(debug_assertions))]
    {
        "https://blossom.primal.net".to_string()
    }
}

#[frb]
pub fn group_id_to_string(group_id: &GroupId) -> String {
    ::hex::encode(group_id.as_slice())
}

#[frb]
pub fn group_id_from_string(group_id: &str) -> Result<GroupId, ApiError> {
    let bytes = ::hex::decode(group_id)?;
    Ok(GroupId::from_slice(&bytes))
}

#[frb(sync)]
pub fn theme_mode_light() -> ThemeMode {
    ThemeMode::Light
}

#[frb(sync)]
pub fn theme_mode_dark() -> ThemeMode {
    ThemeMode::Dark
}

#[frb(sync)]
pub fn theme_mode_system() -> ThemeMode {
    ThemeMode::System
}

#[frb(sync)]
pub fn theme_mode_to_string(theme_mode: &ThemeMode) -> String {
    match theme_mode {
        ThemeMode::Light => "light".to_string(),
        ThemeMode::Dark => "dark".to_string(),
        ThemeMode::System => "system".to_string(),
    }
}

#[frb(sync)]
pub fn language_english() -> Language {
    Language::English
}

#[frb(sync)]
pub fn language_spanish() -> Language {
    Language::Spanish
}

#[frb(sync)]
pub fn language_french() -> Language {
    Language::French
}

#[frb(sync)]
pub fn language_german() -> Language {
    Language::German
}

#[frb(sync)]
pub fn language_italian() -> Language {
    Language::Italian
}

#[frb(sync)]
pub fn language_portuguese() -> Language {
    Language::Portuguese
}

#[frb(sync)]
pub fn language_russian() -> Language {
    Language::Russian
}

#[frb(sync)]
pub fn language_turkish() -> Language {
    Language::Turkish
}

#[frb(sync)]
pub fn language_system() -> Language {
    Language::System
}

#[frb(sync)]
pub fn language_to_string(language: &Language) -> String {
    match language {
        Language::System => "system".to_string(),
        Language::English => "en".to_string(),
        Language::Spanish => "es".to_string(),
        Language::French => "fr".to_string(),
        Language::German => "de".to_string(),
        Language::Italian => "it".to_string(),
        Language::Portuguese => "pt".to_string(),
        Language::Russian => "ru".to_string(),
        Language::Turkish => "tr".to_string(),
    }
}

/// Build a `nostr:nevent1...` URI from a hex event ID and author pubkey (NIP-C7).
#[frb(sync)]
pub fn event_id_to_nevent_uri(event_id_hex: &str, pubkey_hex: &str) -> Result<String, ApiError> {
    let event_id = EventId::from_hex(event_id_hex)?;
    let pubkey = PublicKey::from_hex(pubkey_hex)?;
    let nip19 = Nip19Event::new(event_id).author(pubkey);
    let bech32 = nip19.to_bech32().map_err(|e| ApiError::Other {
        message: e.to_string(),
    })?;
    Ok(format!("nostr:{bech32}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_EVENT_ID: &str = "121cfe5d97b0a8fa5bbd53754c4b440283dfd887dc8ba9807ea21fcb2e054cc9";
    const TEST_PUBKEY: &str = "75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7";

    #[test]
    fn test_event_id_to_nevent_uri_produces_valid_uri() {
        let uri = event_id_to_nevent_uri(TEST_EVENT_ID, TEST_PUBKEY).unwrap();
        assert!(uri.starts_with("nostr:nevent1"));
    }

    #[test]
    fn test_event_id_to_nevent_uri_roundtrip() {
        let uri = event_id_to_nevent_uri(TEST_EVENT_ID, TEST_PUBKEY).unwrap();
        let bech32_part = uri.strip_prefix("nostr:").unwrap();
        let nip19 = Nip19Event::from_bech32(bech32_part).unwrap();

        assert_eq!(nip19.event_id.to_hex(), TEST_EVENT_ID);
        assert_eq!(nip19.author.unwrap().to_hex(), TEST_PUBKEY);
    }

    #[test]
    fn test_event_id_to_nevent_uri_invalid_event_id() {
        let result = event_id_to_nevent_uri("not_hex", TEST_PUBKEY);
        assert!(result.is_err());
    }

    #[test]
    fn test_event_id_to_nevent_uri_invalid_pubkey() {
        let result = event_id_to_nevent_uri(TEST_EVENT_ID, "not_hex");
        assert!(result.is_err());
    }
}

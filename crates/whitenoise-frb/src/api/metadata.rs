//! Metadata management for Nostr user profiles in White Noise.
//!
//! This module provides Flutter-compatible data structures and utilities for handling
//! Nostr metadata (user profile information) following the Nostr protocol standards.
//!
//! # Key Features
//! - Flutter-compatible representation of Nostr metadata
//! - Conversion between core library types and Flutter-compatible types
//! - Custom field management with proper serialization
//! - Standard Nostr metadata fields (NIP-01 compliant)

use flutter_rust_bridge::frb;
pub use nostr_sdk::Metadata;
use std::collections::HashMap;

/// Flutter-compatible representation of user metadata following Nostr protocol standards.
///
/// This struct provides a bridge between the core library's `Metadata` type and Flutter's
/// type system. The `custom` field is kept private to avoid flutter_rust_bridge
/// auto-generation issues and is accessed through getter/setter methods.
///
/// # Nostr Metadata Fields
/// Most fields correspond to standard Nostr metadata as defined in NIP-01 and related NIPs.
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct FlutterMetadata {
    /// User's name/username
    pub name: Option<String>,
    /// Display name for the user (can be different from name)
    pub display_name: Option<String>,
    /// User's bio/description
    pub about: Option<String>,
    /// URL to user's profile picture
    pub picture: Option<String>,
    /// URL to user's banner/header image
    pub banner: Option<String>,
    /// User's website URL
    pub website: Option<String>,
    /// NIP-05 verification identifier (e.g., "user@domain.com")
    pub nip05: Option<String>,
    /// Lightning Network address in older format
    pub lud06: Option<String>,
    /// Lightning Network address in newer format
    pub lud16: Option<String>,
    /// Additional custom metadata fields
    pub custom: HashMap<String, String>,
}

/// Implements conversion from the core `Metadata` type to our Flutter-compatible `FlutterMetadata` type.
///
/// This conversion handles the transformation of the `custom` field from `BTreeMap<String, serde_json::Value>`
/// to `HashMap<String, String>`, converting JSON values to their string representations.
/// Arrays and objects are serialized to JSON strings.
impl From<Metadata> for FlutterMetadata {
    fn from(metadata: Metadata) -> Self {
        // Convert BTreeMap<String, serde_json::Value> to HashMap<String, String>
        let custom_string_map = metadata
            .custom
            .iter()
            .map(|(key, value)| {
                let string_value = match value {
                    serde_json::Value::String(s) => s.clone(),
                    _ => serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()),
                };
                (key.clone(), string_value)
            })
            .collect();

        FlutterMetadata {
            name: metadata.name,
            display_name: metadata.display_name,
            about: metadata.about,
            picture: metadata.picture,
            banner: metadata.banner,
            website: metadata.website,
            nip05: metadata.nip05,
            lud06: metadata.lud06,
            lud16: metadata.lud16,
            custom: custom_string_map,
        }
    }
}

/// Implements conversion from a reference to the core `Metadata` type to our Flutter-compatible `FlutterMetadata` type.
///
/// This is useful when you have a reference to a `Metadata` object and want to convert it
/// without taking ownership.
impl From<&Metadata> for FlutterMetadata {
    fn from(metadata: &Metadata) -> Self {
        // Convert BTreeMap<String, serde_json::Value> to HashMap<String, String>
        let custom_string_map = metadata
            .custom
            .iter()
            .map(|(key, value)| {
                let string_value = match value {
                    serde_json::Value::String(s) => s.clone(),
                    _ => serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()),
                };
                (key.clone(), string_value)
            })
            .collect();

        FlutterMetadata {
            name: metadata.name.clone(),
            display_name: metadata.display_name.clone(),
            about: metadata.about.clone(),
            picture: metadata.picture.clone(),
            banner: metadata.banner.clone(),
            website: metadata.website.clone(),
            nip05: metadata.nip05.clone(),
            lud06: metadata.lud06.clone(),
            lud16: metadata.lud16.clone(),
            custom: custom_string_map,
        }
    }
}

/// Implements conversion from our Flutter-compatible `FlutterMetadata` type back to the core `Metadata` type.
///
/// This conversion reverses the process performed by the `From<Metadata>` implementation,
/// attempting to parse string values back to JSON where possible, falling back to
/// string values when parsing fails.
impl From<FlutterMetadata> for Metadata {
    fn from(metadata_data: FlutterMetadata) -> Self {
        // Convert HashMap<String, String> back to BTreeMap<String, serde_json::Value>
        let custom_value_map = metadata_data
            .custom
            .iter()
            .map(|(key, value)| {
                // Try to parse as JSON first, fall back to string
                let json_value = serde_json::from_str(value)
                    .unwrap_or_else(|_| serde_json::Value::String(value.clone()));
                (key.clone(), json_value)
            })
            .collect();

        Metadata {
            name: metadata_data.name,
            display_name: metadata_data.display_name,
            about: metadata_data.about,
            picture: metadata_data.picture,
            banner: metadata_data.banner,
            website: metadata_data.website,
            nip05: metadata_data.nip05,
            lud06: metadata_data.lud06,
            lud16: metadata_data.lud16,
            custom: custom_value_map,
        }
    }
}

/// Implements conversion from a reference to our Flutter-compatible `FlutterMetadata` type back to the core `Metadata` type.
///
/// This is useful when you have a reference to a `FlutterMetadata` object and want to convert it
/// without taking ownership.
impl From<&FlutterMetadata> for Metadata {
    fn from(metadata_data: &FlutterMetadata) -> Self {
        // Convert HashMap<String, String> back to BTreeMap<String, serde_json::Value>
        let custom_value_map = metadata_data
            .custom
            .iter()
            .map(|(key, value)| {
                // Try to parse as JSON first, fall back to string
                let json_value = serde_json::from_str(value)
                    .unwrap_or_else(|_| serde_json::Value::String(value.clone()));
                (key.clone(), json_value)
            })
            .collect();

        Metadata {
            name: metadata_data.name.clone(),
            display_name: metadata_data.display_name.clone(),
            about: metadata_data.about.clone(),
            picture: metadata_data.picture.clone(),
            banner: metadata_data.banner.clone(),
            website: metadata_data.website.clone(),
            nip05: metadata_data.nip05.clone(),
            lud06: metadata_data.lud06.clone(),
            lud16: metadata_data.lud16.clone(),
            custom: custom_value_map,
        }
    }
}

use std::sync::Arc;

use crate::whitenoise::media_files::MediaFile;
use crate::{Account, Whitenoise, WhitenoiseError};
use mdk_core::prelude::group_types::Group;
use nostr_sdk::prelude::RelayUrl;
use std::collections::HashMap;

/// Per-scenario state shared across test cases.
///
/// The two relay fields are derived from the live [`WhitenoiseConfig`] at
/// construction time so test scaffolding inherits the same relay configuration
/// the binary passed in. They are intentionally kept distinct so a private-relay
/// deployment can configure them differently without test scaffolding falling
/// back to a compile-time relay choice.
///
/// [`WhitenoiseConfig`]: crate::WhitenoiseConfig
#[derive(Clone)]
pub struct ScenarioContext {
    pub whitenoise: Arc<Whitenoise>,
    /// Relays this Whitenoise instance gossips on for other users' metadata,
    /// follow lists, and discovery. Mirrors `WhitenoiseConfig::discovery_relays`.
    pub discovery_relays: Vec<String>,
    /// Relays adopted as a freshly-created account's NIP-65, Inbox, and
    /// KeyPackage lists, and the initial lookup target for login flows. Mirrors
    /// `WhitenoiseConfig::default_account_relays`.
    pub default_account_relays: Vec<String>,
    pub accounts: HashMap<String, Account>,
    pub groups: HashMap<String, Group>,
    pub messages_ids: HashMap<String, String>,
    pub media_files: HashMap<String, MediaFile>,
    pub tests_count: u32,
    pub tests_passed: u32,
}

impl ScenarioContext {
    pub fn new(whitenoise: Arc<Whitenoise>) -> Self {
        let config = whitenoise.config();
        let discovery_relays = config
            .discovery_relays
            .iter()
            .map(|url| url.to_string())
            .collect();
        let default_account_relays = config
            .default_account_relays
            .iter()
            .map(|url| url.to_string())
            .collect();
        Self {
            whitenoise,
            discovery_relays,
            default_account_relays,
            accounts: HashMap::new(),
            groups: HashMap::new(),
            messages_ids: HashMap::new(),
            media_files: HashMap::new(),
            tests_count: 0,
            tests_passed: 0,
        }
    }

    pub fn add_account(&mut self, name: &str, account: Account) {
        self.accounts.insert(name.to_string(), account);
    }

    pub fn get_account(&self, name: &str) -> Result<&Account, WhitenoiseError> {
        self.accounts
            .get(name)
            .ok_or(WhitenoiseError::AccountNotFound)
    }

    pub fn add_group(&mut self, name: &str, group: Group) {
        self.groups.insert(name.to_string(), group);
    }

    pub fn get_group(&self, name: &str) -> Result<&Group, WhitenoiseError> {
        self.groups.get(name).ok_or(WhitenoiseError::GroupNotFound)
    }

    pub fn add_media_file(&mut self, name: &str, media_file: MediaFile) {
        self.media_files.insert(name.to_string(), media_file);
    }

    pub fn get_media_file(&self, name: &str) -> Result<&MediaFile, WhitenoiseError> {
        self.media_files.get(name).ok_or_else(|| {
            WhitenoiseError::Configuration(format!("Media file '{}' not found in context", name))
        })
    }

    pub fn add_message_id(&mut self, name: &str, message_id: String) {
        self.messages_ids.insert(name.to_string(), message_id);
    }

    pub fn get_message_id(&self, message_id: &str) -> Result<&String, WhitenoiseError> {
        self.messages_ids.get(message_id).ok_or_else(|| {
            WhitenoiseError::Configuration(format!(
                "Message ID '{}' not found in context",
                message_id
            ))
        })
    }

    pub fn record_test(&mut self, passed: bool) {
        self.tests_count += 1;
        if passed {
            self.tests_passed += 1;
        }
    }

    /// Discovery-plane relays as parsed `RelayUrl` instances.
    ///
    /// Use this when seeding fixture data the Whitenoise instance should find
    /// through its discovery plane (metadata, follow lists, user discovery).
    pub fn discovery_relay_urls(&self) -> Vec<RelayUrl> {
        self.discovery_relays
            .iter()
            .filter_map(|url| RelayUrl::parse(url).ok())
            .collect()
    }

    /// Default-account relays as parsed `RelayUrl` instances.
    ///
    /// Use this for login fixtures (publish destination and NIP-65 / Inbox /
    /// KeyPackage event payloads) and for MLS group relay configs.
    pub fn default_account_relay_urls(&self) -> Vec<RelayUrl> {
        self.default_account_relays
            .iter()
            .filter_map(|url| RelayUrl::parse(url).ok())
            .collect()
    }
}

//! Marmot protocol types used by the WhiteNoise public API.
//!
//! This module owns caller-facing protocol input types so upstream Darkmatter
//! engine types do not leak into the public WhiteNoise API.

use std::collections::BTreeSet;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::str::FromStr;

use cgka_traits::app_components::NostrRoutingV1;
use nostr_sdk::{EventId, PublicKey, RelayUrl, Timestamp};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::{Zeroize, ZeroizeOnDrop};

pub mod group_types {
    pub use super::{Group, GroupState, SelfUpdateState};
}

/// MLS group identifier used by WhiteNoise public APIs.
///
/// This is an opaque byte string. It is not a Nostr routing id.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GroupId(Vec<u8>);

impl GroupId {
    /// Creates a group id from owned bytes.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// Creates a group id by copying a byte slice.
    pub fn from_slice(bytes: &[u8]) -> Self {
        Self(bytes.to_vec())
    }

    /// Returns the raw group id bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Returns a copy of the raw group id bytes.
    pub fn to_vec(&self) -> Vec<u8> {
        self.0.clone()
    }

    /// Consumes the group id and returns the raw bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl AsRef<[u8]> for GroupId {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl fmt::Debug for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GroupId([REDACTED])")
    }
}

impl fmt::Display for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.as_slice()))
    }
}

impl From<cgka_traits::types::GroupId> for GroupId {
    fn from(value: cgka_traits::types::GroupId) -> Self {
        Self::new(value.into_bytes())
    }
}

impl From<&cgka_traits::types::GroupId> for GroupId {
    fn from(value: &cgka_traits::types::GroupId) -> Self {
        Self::from_slice(value.as_slice())
    }
}

impl From<GroupId> for cgka_traits::types::GroupId {
    fn from(value: GroupId) -> Self {
        Self::new(value.into_bytes())
    }
}

impl From<&GroupId> for cgka_traits::types::GroupId {
    fn from(value: &GroupId) -> Self {
        Self::new(value.as_slice().to_vec())
    }
}

/// A value that is zeroized when dropped and redacted in debug output.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, ZeroizeOnDrop)]
pub struct Secret<T>
where
    T: Zeroize,
{
    #[zeroize(drop)]
    value: T,
}

impl<T> Secret<T>
where
    T: Zeroize,
{
    /// Wraps a secret value.
    pub fn new(value: T) -> Self {
        Self { value }
    }

    /// Explicitly exposes the plaintext value for deliberate serialization.
    #[must_use = "PlaintextSecret is only useful when passed to a serializer"]
    pub fn expose_for_serialization(&self) -> PlaintextSecret<'_, T> {
        PlaintextSecret(&self.value)
    }
}

impl<T> AsMut<T> for Secret<T>
where
    T: Zeroize,
{
    fn as_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl<T> AsRef<T> for Secret<T>
where
    T: Zeroize,
{
    fn as_ref(&self) -> &T {
        &self.value
    }
}

impl<T> Deref for Secret<T>
where
    T: Zeroize,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> DerefMut for Secret<T>
where
    T: Zeroize,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

impl<T> fmt::Debug for Secret<T>
where
    T: Zeroize,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl<T> Serialize for Secret<T>
where
    T: Zeroize,
{
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Err(serde::ser::Error::custom(
            "Secret values cannot be serialized; use Secret::expose_for_serialization()",
        ))
    }
}

impl<'de, T> Deserialize<'de> for Secret<T>
where
    T: Zeroize + Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Self::new)
    }
}

/// Opt-in plaintext serialization wrapper for [`Secret`].
pub struct PlaintextSecret<'a, T>(&'a T);

impl<T> fmt::Debug for PlaintextSecret<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PlaintextSecret(***)")
    }
}

impl<T> Serialize for PlaintextSecret<'_, T>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

/// Tracks whether and when the account completed a required self-update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SelfUpdateState {
    /// A self-update is required before the group should be treated as current.
    Required,
    /// The last self-update completed at the given timestamp.
    CompletedAt(Timestamp),
}

impl Serialize for SelfUpdateState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Required => serializer.serialize_u64(0),
            Self::CompletedAt(timestamp) => serializer.serialize_u64(timestamp.as_secs()),
        }
    }
}

impl<'de> Deserialize<'de> for SelfUpdateState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let seconds = u64::deserialize(deserializer)?;
        match seconds {
            0 => Ok(Self::Required),
            seconds => Ok(Self::CompletedAt(Timestamp::from_secs(seconds))),
        }
    }
}

/// Current local state of a group for this account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GroupState {
    /// The group is active.
    Active,
    /// The account has left, declined, or otherwise made the group inactive.
    Inactive,
    /// The group is pending user confirmation.
    Pending,
    /// The group cannot be used until an explicit repair path runs.
    Unrecoverable,
}

impl GroupState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Inactive => "inactive",
            Self::Pending => "pending",
            Self::Unrecoverable => "unrecoverable",
        }
    }
}

impl fmt::Display for GroupState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for GroupState {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "active" => Ok(Self::Active),
            "inactive" => Ok(Self::Inactive),
            "pending" => Ok(Self::Pending),
            "unrecoverable" => Ok(Self::Unrecoverable),
            other => Err(format!("Invalid group state: {other}")),
        }
    }
}

impl Serialize for GroupState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for GroupState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).map_err(serde::de::Error::custom)
    }
}

/// Group metadata projected into WhiteNoise public APIs.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Group {
    /// MLS group identifier.
    pub mls_group_id: GroupId,
    /// Nostr routing group id used by kind `445` messages.
    pub nostr_group_id: [u8; 32],
    /// Group name.
    pub name: String,
    /// Group description.
    pub description: String,
    /// SHA-256 hash of the group image.
    pub image_hash: Option<[u8; 32]>,
    /// Secret key used to decrypt the group image.
    pub image_key: Option<Secret<[u8; 32]>>,
    /// Nonce used to decrypt the group image.
    pub image_nonce: Option<Secret<[u8; 12]>>,
    /// Group admin account public keys.
    pub admin_pubkeys: BTreeSet<PublicKey>,
    /// Event id of the last visible group message.
    pub last_message_id: Option<EventId>,
    /// Sender timestamp of the last visible group message.
    pub last_message_at: Option<Timestamp>,
    /// Local processing timestamp of the last visible group message.
    pub last_message_processed_at: Option<Timestamp>,
    /// Group epoch.
    pub epoch: u64,
    /// Group state for this account.
    pub state: GroupState,
    /// Self-update tracking state.
    pub self_update_state: SelfUpdateState,
    /// Optional disappearing-message duration in seconds.
    pub disappearing_message_secs: Option<u64>,
}

impl fmt::Debug for Group {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Group").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MarmotCreatedGroupProjection {
    pub(crate) group_id: cgka_traits::types::GroupId,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) epoch: u64,
    pub(crate) routing: NostrRoutingV1,
    pub(crate) admin_pubkeys: BTreeSet<PublicKey>,
    pub(crate) member_pubkeys: BTreeSet<PublicKey>,
    pub(crate) self_update_completed_at_secs: u64,
    pub(crate) disappearing_message_secs: Option<u64>,
}

/// Configuration data for creating a Marmot group.
///
/// This intentionally mirrors the group configuration shape while giving
/// WhiteNoise ownership of the public input contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupConfig {
    /// Group name.
    pub name: String,
    /// Group description.
    pub description: String,
    /// SHA-256 hash of the encrypted group image.
    pub image_hash: Option<[u8; 32]>,
    /// Key used to decrypt the image.
    pub image_key: Option<[u8; 32]>,
    /// Nonce used to decrypt the image.
    pub image_nonce: Option<[u8; 12]>,
    /// Relays used by the group.
    pub relays: Vec<RelayUrl>,
    /// Group admins.
    pub admins: Vec<PublicKey>,
    /// Disappearing message duration in seconds.
    ///
    /// `None` disables disappearing messages.
    pub disappearing_message_secs: Option<u64>,
}

/// Configuration for updating group data with optional fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupDataUpdate {
    /// Group name.
    pub name: Option<String>,
    /// Group description.
    pub description: Option<String>,
    /// Image hash. Use `Some(None)` to clear.
    pub image_hash: Option<Option<[u8; 32]>>,
    /// Image decryption key. Use `Some(None)` to clear.
    pub image_key: Option<Option<[u8; 32]>>,
    /// Image decryption nonce. Use `Some(None)` to clear.
    pub image_nonce: Option<Option<[u8; 12]>>,
    /// Image upload key seed. Use `Some(None)` to clear.
    pub image_upload_key: Option<Option<[u8; 32]>>,
    /// Relays used by the group.
    pub relays: Option<Vec<RelayUrl>>,
    /// Group admins.
    pub admins: Option<Vec<PublicKey>>,
    /// Nostr group ID used for message routing.
    pub nostr_group_id: Option<[u8; 32]>,
    /// Disappearing message duration. Use `Some(None)` to disable.
    pub disappearing_message_secs: Option<Option<u64>>,
}

impl GroupConfig {
    /// Creates group configuration data.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        description: String,
        image_hash: Option<[u8; 32]>,
        image_key: Option<[u8; 32]>,
        image_nonce: Option<[u8; 12]>,
        relays: Vec<RelayUrl>,
        admins: Vec<PublicKey>,
        disappearing_message_secs: Option<u64>,
    ) -> Self {
        Self {
            name,
            description,
            image_hash,
            image_key,
            image_nonce,
            relays,
            admins,
            disappearing_message_secs,
        }
    }
}

impl GroupDataUpdate {
    /// Creates an empty group-data update.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the name to update.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the description to update.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Sets or clears the image hash.
    pub fn image_hash(mut self, image_hash: Option<[u8; 32]>) -> Self {
        self.image_hash = Some(image_hash);
        self
    }

    /// Sets or clears the image decryption key.
    pub fn image_key(mut self, image_key: Option<[u8; 32]>) -> Self {
        self.image_key = Some(image_key);
        self
    }

    /// Sets or clears the image decryption nonce.
    pub fn image_nonce(mut self, image_nonce: Option<[u8; 12]>) -> Self {
        self.image_nonce = Some(image_nonce);
        self
    }

    /// Sets or clears the image upload key seed.
    pub fn image_upload_key(mut self, image_upload_key: Option<[u8; 32]>) -> Self {
        self.image_upload_key = Some(image_upload_key);
        self
    }

    /// Sets the group relays.
    pub fn relays(mut self, relays: Vec<RelayUrl>) -> Self {
        self.relays = Some(relays);
        self
    }

    /// Sets the group admins.
    pub fn admins(mut self, admins: Vec<PublicKey>) -> Self {
        self.admins = Some(admins);
        self
    }

    /// Sets the Nostr routing group ID.
    pub fn nostr_group_id(mut self, nostr_group_id: [u8; 32]) -> Self {
        self.nostr_group_id = Some(nostr_group_id);
        self
    }

    /// Sets or disables the disappearing-message duration.
    pub fn disappearing_message_secs(mut self, disappearing_message_secs: Option<u64>) -> Self {
        self.disappearing_message_secs = Some(disappearing_message_secs);
        self
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::RelayUrl;
    use nostr_sdk::prelude::Keys;

    use super::{GroupConfig, GroupDataUpdate};

    #[test]
    fn group_id_is_whitenoise_owned_byte_identifier() {
        let group_id = super::GroupId::new(vec![0xab, 0xcd, 0xef]);

        assert_eq!(group_id.as_slice(), &[0xab, 0xcd, 0xef]);
        assert_eq!(group_id.to_vec(), vec![0xab, 0xcd, 0xef]);
        assert_eq!(group_id.clone().into_bytes(), vec![0xab, 0xcd, 0xef]);
        assert_eq!(group_id.to_string(), "abcdef");
        assert_eq!(format!("{group_id:?}"), "GroupId([REDACTED])");
    }

    #[test]
    fn group_config_is_whitenoise_owned_value_type() {
        let admin = Keys::generate().public_key();
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let config = GroupConfig::new(
            "group".to_string(),
            "description".to_string(),
            Some([1; 32]),
            Some([2; 32]),
            Some([3; 12]),
            vec![relay.clone()],
            vec![admin],
            Some(60),
        );

        assert_eq!(config, config.clone());
        assert_eq!(config.name, "group");
        assert_eq!(config.description, "description");
        assert_eq!(config.image_hash, Some([1; 32]));
        assert_eq!(config.image_key, Some([2; 32]));
        assert_eq!(config.image_nonce, Some([3; 12]));
        assert_eq!(config.relays, vec![relay]);
        assert_eq!(config.admins, vec![admin]);
        assert_eq!(config.disappearing_message_secs, Some(60));
    }

    #[test]
    fn group_data_update_builders_preserve_set_and_clear_semantics() {
        let admin = Keys::generate().public_key();
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let update = GroupDataUpdate::new()
            .name("new name")
            .description("new description")
            .image_hash(None)
            .image_key(Some([1; 32]))
            .image_nonce(None)
            .image_upload_key(Some([2; 32]))
            .relays(vec![relay.clone()])
            .admins(vec![admin])
            .nostr_group_id([3; 32])
            .disappearing_message_secs(None);

        assert_eq!(update.image_hash, Some(None));
        assert_eq!(update.image_nonce, Some(None));
        assert_eq!(update.name.as_deref(), Some("new name"));
        assert_eq!(update.description.as_deref(), Some("new description"));
        assert_eq!(update.image_key, Some(Some([1; 32])));
        assert_eq!(update.image_upload_key, Some(Some([2; 32])));
        assert_eq!(update.relays, Some(vec![relay]));
        assert_eq!(update.admins, Some(vec![admin]));
        assert_eq!(update.nostr_group_id, Some([3; 32]));
        assert_eq!(update.disappearing_message_secs, Some(None));
    }
}

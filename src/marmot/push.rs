//! MIP-05 push-notification protocol primitives.
//!
//! WhiteNoise owns these codecs. The wire details follow the current
//! Darkmatter push-notification spec for token encryption, JSON group-gossip
//! payloads, and notification trigger payloads.

use std::collections::BTreeSet;
use std::fmt;

use base64ct::{Base64, Encoding as _};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use hkdf::Hkdf;
use nostr_sdk::secp256k1::{Parity, PublicKey as SecpPublicKey, XOnlyPublicKey, ecdh};
use nostr_sdk::{
    Event, EventId, Keys, Kind, PublicKey, RelayUrl, SecretKey, Tag, TagKind, Timestamp,
    UnsignedEvent,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// MIP-05 `kind:446` notification request rumor.
pub const NOTIFICATION_REQUEST_KIND: u16 = 446;
/// MIP-05 `kind:447` token request rumor.
pub const TOKEN_REQUEST_KIND: u16 = 447;
/// MIP-05 `kind:448` token list response rumor.
pub const TOKEN_LIST_RESPONSE_KIND: u16 = 448;
/// MIP-05 `kind:449` token removal rumor.
pub const TOKEN_REMOVAL_KIND: u16 = 449;

/// Recommended maximum number of encrypted tokens per notification request.
///
/// Capped to stay within the NIP-44 plaintext limit once the rumor content is
/// base64-encoded and JSON-serialized.
pub const MAX_NOTIFICATION_REQUEST_TOKENS: usize = 25;

/// MIP-05 padded token plaintext length.
pub const TOKEN_PLAINTEXT_LEN: usize = 1024;
/// MIP-05 encrypted token length.
pub const ENCRYPTED_TOKEN_LEN: usize = 1084;

const EPHEMERAL_PUBKEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const AEAD_TAG_LEN: usize = 16;
const TOKEN_CIPHERTEXT_LEN: usize = TOKEN_PLAINTEXT_LEN + AEAD_TAG_LEN;
const CIPHERTEXT_OFFSET: usize = EPHEMERAL_PUBKEY_LEN + NONCE_LEN;

const _: [(); ENCRYPTED_TOKEN_LEN] = [(); EPHEMERAL_PUBKEY_LEN + NONCE_LEN + TOKEN_CIPHERTEXT_LEN];

const TOKEN_TAG_NAME: &str = "token";
const VERSION_TAG_NAME: &str = "v";
const MIP05_VERSION: &str = "mip05-v1";
const TOKEN_ENCRYPTION_SALT: &[u8] = b"mip05-v1";
const TOKEN_ENCRYPTION_INFO: &[u8] = b"mip05-token-encryption";

/// Errors that can occur while building, parsing, encrypting, or decrypting
/// MIP-05 protocol objects.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Mip05Error {
    #[error("invalid notification platform")]
    InvalidNotificationPlatform,
    #[error("invalid device token length")]
    InvalidDeviceTokenLength,
    #[error("invalid MIP-05 token padding length")]
    InvalidTokenPaddingLength,
    #[error("device token is too large")]
    DeviceTokenTooLarge,
    #[error("invalid MIP-05 token plaintext length")]
    InvalidTokenPlaintextLength,
    #[error("invalid MIP-05 token length")]
    InvalidTokenLength,
    #[error("invalid encrypted token length")]
    InvalidEncryptedTokenLength,
    #[error("invalid encrypted token base64")]
    InvalidEncryptedTokenBase64,
    #[error("invalid encrypted token public key")]
    InvalidEncryptedTokenPublicKey,
    #[error("invalid encrypted token nonce")]
    InvalidEncryptedTokenNonce,
    #[error("failed to derive MIP-05 encryption key")]
    KeyDerivationFailed,
    #[error("failed to encrypt push token")]
    EncryptionFailed,
    #[error("failed to decrypt encrypted token")]
    DecryptionFailed,
    #[error("invalid encrypted token ciphertext length")]
    InvalidCiphertextLength,
    #[error("unsupported MIP-05 rumor kind")]
    UnexpectedRumorKind,
    #[error("malformed MIP-05 JSON payload")]
    MalformedJsonPayload,
    #[error("unsupported MIP-05 version")]
    UnsupportedVersion,
    #[error("invalid MIP-05 member public key")]
    InvalidMemberPublicKey,
    #[error("invalid MIP-05 token fingerprint")]
    InvalidTokenFingerprint,
    #[error("MIP-05 Darkmatter token gossip is missing platform metadata")]
    MissingTokenPlatform,
    #[error("MIP-05 Darkmatter token gossip is missing token fingerprint metadata")]
    MissingTokenFingerprint,
    #[error("missing MIP-05 source member")]
    MissingSourceMember,
    #[error("MIP-05 rumors must have empty content")]
    NonEmptyContent,
    #[error("token request must include at least one token")]
    TokenRequestMustIncludeToken,
    #[error("token request contains unsupported tags")]
    UnsupportedTokenRequestTags,
    #[error("token list response must include at least one token")]
    TokenListResponseMustIncludeToken,
    #[error("token list response must contain exactly one event reference")]
    TokenListResponseMustContainSingleEventReference,
    #[error("token list response contains unsupported tags")]
    UnsupportedTokenListResponseTags,
    #[error("token removal rumors must not contain tags")]
    TokenRemovalMustNotContainTags,
    #[error("invalid token tag shape")]
    InvalidTokenTagShape,
    #[error("invalid notification server public key")]
    InvalidNotificationServerPublicKey,
    #[error("invalid notification relay hint")]
    InvalidNotificationRelayHint,
    #[error("invalid MIP-05 leaf index")]
    InvalidLeafIndex,
    #[error("duplicate MIP-05 leaf index")]
    DuplicateLeafIndex,
    #[error("missing event reference")]
    MissingEventReference,
    #[error("invalid event reference")]
    InvalidEventReference,
    #[error("notification request must include at least one token")]
    NotificationRequestMustIncludeToken,
    #[error("duplicate encrypted token in notification request batch")]
    DuplicateEncryptedToken,
    #[error("failed to encrypt notification request")]
    NotificationRequestEncryptionFailed,
    #[error("failed to sign notification request seal")]
    NotificationRequestSealFailed,
    #[error("failed to build notification request gift wrap")]
    NotificationRequestGiftWrapFailed,
}

/// Supported push-notification platforms for MIP-05.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationPlatform {
    /// Apple Push Notification service.
    Apns,
    /// Firebase Cloud Messaging.
    Fcm,
}

impl NotificationPlatform {
    /// Convert the platform to the wire-format byte value.
    pub const fn as_byte(self) -> u8 {
        match self {
            Self::Apns => 0x01,
            Self::Fcm => 0x02,
        }
    }

    /// Parse a platform from its wire-format byte value.
    pub fn from_byte(value: u8) -> Result<Self, Mip05Error> {
        match value {
            0x01 => Ok(Self::Apns),
            0x02 => Ok(Self::Fcm),
            _ => Err(Mip05Error::InvalidNotificationPlatform),
        }
    }

    /// Convert the platform to the Darkmatter JSON payload string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Apns => "apns",
            Self::Fcm => "fcm",
        }
    }

    /// Parse a platform from the Darkmatter JSON payload string.
    pub fn from_platform_str(value: &str) -> Result<Self, Mip05Error> {
        match value {
            "apns" => Ok(Self::Apns),
            "fcm" => Ok(Self::Fcm),
            _ => Err(Mip05Error::InvalidNotificationPlatform),
        }
    }

    /// Maximum device token length that fits the MIP-05 wire format.
    pub const MAX_DEVICE_TOKEN_LEN: usize = TOKEN_PLAINTEXT_LEN - 3;
}

/// Stable token fingerprint used by Darkmatter JSON push-token gossip.
pub fn push_token_fingerprint(platform: NotificationPlatform, device_token: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update([platform.as_byte()]);
    hasher.update(device_token);
    let digest = hex::encode(hasher.finalize());
    format!("sha256:{}", &digest[..24])
}

/// Parsed MIP-05 token plaintext.
#[derive(Clone, PartialEq, Eq, Hash, Zeroize, ZeroizeOnDrop)]
pub struct PushTokenPlaintext {
    #[zeroize(skip)]
    platform: NotificationPlatform,
    device_token: Vec<u8>,
}

impl PushTokenPlaintext {
    /// Construct a validated token plaintext.
    pub fn new(platform: NotificationPlatform, device_token: Vec<u8>) -> Result<Self, Mip05Error> {
        if device_token.is_empty()
            || device_token.len() > NotificationPlatform::MAX_DEVICE_TOKEN_LEN
        {
            return Err(Mip05Error::InvalidDeviceTokenLength);
        }
        Ok(Self {
            platform,
            device_token,
        })
    }

    /// Get the platform identifier.
    pub const fn platform(&self) -> NotificationPlatform {
        self.platform
    }

    /// Get the raw device token bytes.
    pub fn device_token(&self) -> &[u8] {
        &self.device_token
    }

    fn encode_padded(&self, padding: &[u8]) -> Result<[u8; TOKEN_PLAINTEXT_LEN], Mip05Error> {
        let padding_len = TOKEN_PLAINTEXT_LEN
            .checked_sub(3 + self.device_token.len())
            .ok_or(Mip05Error::InvalidTokenPaddingLength)?;
        if padding.len() != padding_len {
            return Err(Mip05Error::InvalidTokenPaddingLength);
        }

        let token_len_u16 =
            u16::try_from(self.device_token.len()).map_err(|_| Mip05Error::DeviceTokenTooLarge)?;
        let mut bytes = [0u8; TOKEN_PLAINTEXT_LEN];
        bytes[0] = self.platform.as_byte();
        bytes[1..3].copy_from_slice(&token_len_u16.to_be_bytes());
        bytes[3..3 + self.device_token.len()].copy_from_slice(&self.device_token);
        bytes[3 + self.device_token.len()..].copy_from_slice(padding);
        Ok(bytes)
    }

    #[cfg(test)]
    fn from_padded_slice(bytes: &[u8]) -> Result<Self, Mip05Error> {
        if bytes.len() != TOKEN_PLAINTEXT_LEN {
            return Err(Mip05Error::InvalidTokenPlaintextLength);
        }

        let platform = NotificationPlatform::from_byte(bytes[0])?;
        let token_len = usize::from(u16::from_be_bytes([bytes[1], bytes[2]]));
        let token_end = 3 + token_len;
        if token_end > TOKEN_PLAINTEXT_LEN {
            return Err(Mip05Error::InvalidTokenLength);
        }

        Self::new(platform, bytes[3..token_end].to_vec())
    }
}

impl fmt::Debug for PushTokenPlaintext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PushTokenPlaintext")
            .field("platform", &self.platform)
            .field("device_token_len", &self.device_token.len())
            .finish()
    }
}

/// Fixed-size encrypted MIP-05 token.
#[derive(Clone, PartialEq, Eq, Hash, Zeroize, ZeroizeOnDrop)]
pub struct EncryptedToken([u8; ENCRYPTED_TOKEN_LEN]);

impl EncryptedToken {
    /// Parse an encrypted token from raw bytes.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, Mip05Error> {
        let token: [u8; ENCRYPTED_TOKEN_LEN] = bytes
            .try_into()
            .map_err(|_| Mip05Error::InvalidEncryptedTokenLength)?;
        Ok(Self(token))
    }

    /// Parse an encrypted token from RFC 4648 base64.
    pub fn from_base64(value: &str) -> Result<Self, Mip05Error> {
        let bytes =
            Base64::decode_vec(value).map_err(|_| Mip05Error::InvalidEncryptedTokenBase64)?;
        Self::from_slice(&bytes)
    }

    /// Return the token as raw bytes.
    pub fn as_bytes(&self) -> &[u8; ENCRYPTED_TOKEN_LEN] {
        &self.0
    }

    /// Encode the token as RFC 4648 base64.
    pub fn to_base64(&self) -> String {
        Base64::encode_string(&self.0)
    }
}

impl From<[u8; ENCRYPTED_TOKEN_LEN]> for EncryptedToken {
    fn from(value: [u8; ENCRYPTED_TOKEN_LEN]) -> Self {
        Self(value)
    }
}

impl fmt::Debug for EncryptedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptedToken")
            .field("len", &ENCRYPTED_TOKEN_LEN)
            .finish()
    }
}

/// Shared `token` tag payload for MIP-05 token exchange events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenTag {
    /// The encrypted token payload.
    pub encrypted_token: EncryptedToken,
    /// Platform metadata carried by current Darkmatter JSON payloads.
    pub platform: Option<NotificationPlatform>,
    /// Redacted stable fingerprint carried by current Darkmatter JSON payloads.
    pub token_fingerprint: Option<String>,
    /// The notification server public key used for encryption.
    pub server_pubkey: PublicKey,
    /// A relay hint where the server's `kind:10050` event can be found.
    pub relay_hint: RelayUrl,
}

/// A `token` tag payload with an explicit MLS leaf index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafTokenTag {
    /// The common token-tag payload.
    pub token_tag: TokenTag,
    /// The owning MLS leaf index.
    pub leaf_index: u32,
}

/// A push token with the member identity required by current Darkmatter JSON
/// token gossip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberTokenTag {
    /// Member/account public key that owns the token.
    pub member_pubkey: PublicKey,
    /// Current push-token member index for the token owner.
    pub leaf_index: u32,
    /// The common token-tag payload.
    pub token_tag: TokenTag,
}

/// Source member metadata carried by Darkmatter JSON push-token updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenSource {
    /// Member/account public key that owns the token.
    pub member_pubkey: PublicKey,
    /// Current push-token member index for the token owner.
    pub leaf_index: u32,
}

/// Typed representation of a `kind:447` token request rumor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRequest {
    /// Source member metadata when carried by the current Darkmatter JSON
    /// payload shape. Legacy empty-content rumors infer this from the MLS
    /// sender leaf.
    pub source: Option<TokenSource>,
    /// Tokens advertised by the requesting device.
    pub tokens: Vec<TokenTag>,
}

/// Typed representation of a `kind:448` token list response rumor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenListResponse {
    /// The `kind:447` rumor this response references.
    pub request_event_id: Option<EventId>,
    /// Known tokens for active group leaves.
    pub tokens: Vec<LeafTokenTag>,
}

/// Typed representation of a `kind:449` token removal rumor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct TokenRemoval {
    /// Member public keys whose token records should be removed. Legacy
    /// empty-content rumors leave this empty and infer the member from the MLS
    /// sender leaf.
    pub member_pubkeys: Vec<PublicKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PushTokenGossipPayload {
    v: String,
    #[serde(default)]
    tokens: Vec<PushTokenGossipEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PushTokenGossipEntry {
    member_id_hex: String,
    leaf_index: u32,
    platform: String,
    token_fingerprint: String,
    server_pubkey_hex: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relay_hint: Option<String>,
    encrypted_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PushTokenRemovalPayload {
    v: String,
    #[serde(default)]
    removals: Vec<PushTokenRemovalEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PushTokenRemovalEntry {
    member_id_hex: String,
    platform: String,
    token_fingerprint: String,
    server_pubkey_hex: String,
}

/// Typed representation of MIP-05 MLS application-message rumors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mip05GroupMessage {
    /// A `kind:447` token request.
    TokenRequest(TokenRequest),
    /// A `kind:448` token list response.
    TokenListResponse(TokenListResponse),
    /// A `kind:449` token removal.
    TokenRemoval(TokenRemoval),
}

/// Ready-to-publish notification requests for a single notification server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationEventBatch {
    /// The notification server receiving these requests.
    pub server_pubkey: PublicKey,
    /// Relay hints seen on the source token tags for this server.
    pub relay_hints: Vec<RelayUrl>,
    /// Gift-wrapped `kind:1059` events ready to publish.
    pub events: Vec<Event>,
}

/// Encrypt a validated push token using the MIP-05 wire format.
pub fn encrypt_push_token(
    server_pubkey: &PublicKey,
    plaintext: &PushTokenPlaintext,
) -> Result<EncryptedToken, Mip05Error> {
    let ephemeral_keys = Keys::generate();
    let token_padding_len = TOKEN_PLAINTEXT_LEN
        .checked_sub(3 + plaintext.device_token().len())
        .ok_or(Mip05Error::InvalidTokenPaddingLength)?;
    let mut nonce = [0u8; NONCE_LEN];
    let mut padding = vec![0u8; token_padding_len];
    rand::rng().fill_bytes(&mut nonce);
    rand::rng().fill_bytes(&mut padding);

    encrypt_push_token_with_materials(
        server_pubkey,
        plaintext,
        ephemeral_keys.secret_key().clone(),
        nonce,
        &padding,
    )
}

/// Decrypt a fixed-size MIP-05 encrypted token.
#[cfg(test)]
pub fn decrypt_push_token(
    server_secret_key: &SecretKey,
    encrypted_token: &EncryptedToken,
) -> Result<PushTokenPlaintext, Mip05Error> {
    let bytes = encrypted_token.as_bytes();
    let ephemeral_pubkey = secp_public_key_from_xonly_bytes(&bytes[..EPHEMERAL_PUBKEY_LEN])
        .map_err(|_| Mip05Error::InvalidEncryptedTokenPublicKey)?;
    let nonce_bytes: [u8; NONCE_LEN] = bytes[EPHEMERAL_PUBKEY_LEN..CIPHERTEXT_OFFSET]
        .try_into()
        .map_err(|_| Mip05Error::InvalidEncryptedTokenNonce)?;
    let ciphertext = &bytes[CIPHERTEXT_OFFSET..];

    let key = derive_encryption_key(server_secret_key, &ephemeral_pubkey)?;
    let cipher = ChaCha20Poly1305::new_from_slice(key.as_slice())
        .map_err(|_| Mip05Error::KeyDerivationFailed)?;
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: ciphertext,
                    aad: b"",
                },
            )
            .map_err(|_| Mip05Error::DecryptionFailed)?,
    );

    PushTokenPlaintext::from_padded_slice(&plaintext)
}

/// Build an unsigned `kind:447` MIP-05 token request rumor.
pub fn build_token_request_rumor(
    pubkey: PublicKey,
    created_at: Timestamp,
    tokens: Vec<TokenTag>,
) -> Result<UnsignedEvent, Mip05Error> {
    if tokens.is_empty() {
        return Err(Mip05Error::TokenRequestMustIncludeToken);
    }

    let mut rumor = UnsignedEvent::new(
        pubkey,
        created_at,
        Kind::from(TOKEN_REQUEST_KIND),
        build_token_request_tags(tokens),
        String::new(),
    );
    rumor.ensure_id();
    Ok(rumor)
}

/// Build a current Darkmatter JSON `kind:447` push-token update app event.
pub fn build_token_update_app_event(
    pubkey: PublicKey,
    created_at: Timestamp,
    member_pubkey: PublicKey,
    leaf_index: u32,
    platform: NotificationPlatform,
    token_fingerprint: &str,
    token: TokenTag,
) -> Result<UnsignedEvent, Mip05Error> {
    validate_token_fingerprint(token_fingerprint)?;
    let payload = PushTokenGossipPayload {
        v: MIP05_VERSION.to_string(),
        tokens: vec![PushTokenGossipEntry::from_token(
            member_pubkey,
            leaf_index,
            platform,
            token_fingerprint,
            token,
        )],
    };
    build_json_app_event(pubkey, created_at, TOKEN_REQUEST_KIND, &payload)
}

/// Build an unsigned `kind:448` MIP-05 token list response rumor.
pub fn build_token_list_response_rumor(
    pubkey: PublicKey,
    created_at: Timestamp,
    request_event_id: EventId,
    tokens: Vec<LeafTokenTag>,
) -> Result<UnsignedEvent, Mip05Error> {
    if tokens.is_empty() {
        return Err(Mip05Error::TokenListResponseMustIncludeToken);
    }
    validate_unique_leaf_indices(&tokens)?;

    let mut tags = build_token_list_response_tags(tokens);
    tags.push(Tag::custom(TagKind::e(), [request_event_id.to_hex()]));

    let mut rumor = UnsignedEvent::new(
        pubkey,
        created_at,
        Kind::from(TOKEN_LIST_RESPONSE_KIND),
        tags,
        String::new(),
    );
    rumor.ensure_id();
    Ok(rumor)
}

/// Build a current Darkmatter JSON `kind:448` push-token list app event.
pub(crate) fn build_token_list_response_app_event(
    pubkey: PublicKey,
    created_at: Timestamp,
    tokens: Vec<MemberTokenTag>,
) -> Result<UnsignedEvent, Mip05Error> {
    if tokens.is_empty() {
        return Err(Mip05Error::TokenListResponseMustIncludeToken);
    }
    validate_unique_member_token_leaf_indices(&tokens)?;

    let tokens = tokens
        .into_iter()
        .map(PushTokenGossipEntry::from_member_token)
        .collect::<Result<Vec<_>, _>>()?;
    let payload = PushTokenGossipPayload {
        v: MIP05_VERSION.to_string(),
        tokens,
    };
    build_json_app_event(pubkey, created_at, TOKEN_LIST_RESPONSE_KIND, &payload)
}

/// Build an unsigned `kind:449` MIP-05 token removal rumor.
pub fn build_token_removal_rumor(pubkey: PublicKey, created_at: Timestamp) -> UnsignedEvent {
    let mut rumor = UnsignedEvent::new(
        pubkey,
        created_at,
        Kind::from(TOKEN_REMOVAL_KIND),
        vec![],
        String::new(),
    );
    rumor.ensure_id();
    rumor
}

/// Build a current Darkmatter JSON `kind:449` push-token removal app event.
pub fn build_token_removal_app_event(
    pubkey: PublicKey,
    created_at: Timestamp,
    member_pubkey: PublicKey,
    platform: NotificationPlatform,
    token_fingerprint: &str,
    server_pubkey: PublicKey,
) -> Result<UnsignedEvent, Mip05Error> {
    validate_token_fingerprint(token_fingerprint)?;
    let payload = PushTokenRemovalPayload {
        v: MIP05_VERSION.to_string(),
        removals: vec![PushTokenRemovalEntry {
            member_id_hex: member_pubkey.to_hex(),
            platform: platform.as_str().to_string(),
            token_fingerprint: token_fingerprint.to_string(),
            server_pubkey_hex: server_pubkey.to_hex(),
        }],
    };
    build_json_app_event(pubkey, created_at, TOKEN_REMOVAL_KIND, &payload)
}

fn build_json_app_event<T>(
    pubkey: PublicKey,
    created_at: Timestamp,
    kind: u16,
    payload: &T,
) -> Result<UnsignedEvent, Mip05Error>
where
    T: Serialize,
{
    let content = serde_json::to_string(payload).map_err(|_| Mip05Error::MalformedJsonPayload)?;
    let mut event = UnsignedEvent::new(
        pubkey,
        created_at,
        Kind::from(kind),
        vec![version_tag()],
        content,
    );
    event.ensure_id();
    Ok(event)
}

/// Parse an MIP-05 token-distribution rumor.
pub fn parse_group_message_rumor(event: &UnsignedEvent) -> Result<Mip05GroupMessage, Mip05Error> {
    match event.kind {
        kind if kind == Kind::from(TOKEN_REQUEST_KIND) => {
            Ok(Mip05GroupMessage::TokenRequest(parse_token_request(event)?))
        }
        kind if kind == Kind::from(TOKEN_LIST_RESPONSE_KIND) => Ok(
            Mip05GroupMessage::TokenListResponse(parse_token_list_response(event)?),
        ),
        kind if kind == Kind::from(TOKEN_REMOVAL_KIND) => {
            Ok(Mip05GroupMessage::TokenRemoval(parse_token_removal(event)?))
        }
        _ => Err(Mip05Error::UnexpectedRumorKind),
    }
}

fn parse_token_request(event: &UnsignedEvent) -> Result<TokenRequest, Mip05Error> {
    if event.content.is_empty() {
        parse_token_request_rumor(event)
    } else {
        parse_token_request_app_event(event)
    }
}

fn parse_token_list_response(event: &UnsignedEvent) -> Result<TokenListResponse, Mip05Error> {
    if event.content.is_empty() {
        parse_token_list_response_rumor(event)
    } else {
        parse_token_list_response_app_event(event)
    }
}

fn parse_token_removal(event: &UnsignedEvent) -> Result<TokenRemoval, Mip05Error> {
    if event.content.is_empty() {
        parse_token_removal_rumor(event)
    } else {
        parse_token_removal_app_event(event)
    }
}

fn parse_token_request_rumor(event: &UnsignedEvent) -> Result<TokenRequest, Mip05Error> {
    validate_empty_content(event)?;

    let mut tokens = Vec::new();
    for tag in event.tags.iter() {
        match tag.kind() {
            TagKind::Custom(name) if name.as_ref() == TOKEN_TAG_NAME => {
                tokens.push(parse_token_tag(tag)?);
            }
            _ => return Err(Mip05Error::UnsupportedTokenRequestTags),
        }
    }

    if tokens.is_empty() {
        return Err(Mip05Error::TokenRequestMustIncludeToken);
    }

    Ok(TokenRequest {
        source: None,
        tokens,
    })
}

fn parse_token_request_app_event(event: &UnsignedEvent) -> Result<TokenRequest, Mip05Error> {
    validate_version_tag(event)?;
    let payload = parse_gossip_payload(&event.content)?;
    if payload.tokens.is_empty() {
        return Err(Mip05Error::TokenRequestMustIncludeToken);
    }
    let source = source_from_gossip_entries(&payload.tokens)?;
    let tokens = payload
        .tokens
        .into_iter()
        .map(PushTokenGossipEntry::into_token_tag)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(TokenRequest {
        source: Some(source),
        tokens,
    })
}

fn parse_token_list_response_rumor(event: &UnsignedEvent) -> Result<TokenListResponse, Mip05Error> {
    validate_empty_content(event)?;

    let mut tokens = Vec::new();
    let mut request_event_id = None;

    for tag in event.tags.iter() {
        match tag.kind() {
            TagKind::Custom(name) if name.as_ref() == TOKEN_TAG_NAME => {
                tokens.push(parse_leaf_token_tag(tag)?);
            }
            kind if kind == TagKind::e() => {
                if request_event_id.is_some() {
                    return Err(Mip05Error::TokenListResponseMustContainSingleEventReference);
                }
                request_event_id = Some(parse_event_reference(tag)?);
            }
            _ => return Err(Mip05Error::UnsupportedTokenListResponseTags),
        }
    }

    if tokens.is_empty() {
        return Err(Mip05Error::TokenListResponseMustIncludeToken);
    }

    validate_unique_leaf_indices(&tokens)?;

    Ok(TokenListResponse {
        request_event_id: Some(
            request_event_id.ok_or(Mip05Error::TokenListResponseMustContainSingleEventReference)?,
        ),
        tokens,
    })
}

fn parse_token_list_response_app_event(
    event: &UnsignedEvent,
) -> Result<TokenListResponse, Mip05Error> {
    validate_version_tag(event)?;
    let payload = parse_gossip_payload(&event.content)?;
    if payload.tokens.is_empty() {
        return Err(Mip05Error::TokenListResponseMustIncludeToken);
    }
    validate_unique_gossip_leaf_indices(&payload.tokens)?;
    let tokens = payload
        .tokens
        .into_iter()
        .map(PushTokenGossipEntry::into_leaf_token_tag)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(TokenListResponse {
        request_event_id: None,
        tokens,
    })
}

fn parse_token_removal_rumor(event: &UnsignedEvent) -> Result<TokenRemoval, Mip05Error> {
    validate_empty_content(event)?;

    if !event.tags.is_empty() {
        return Err(Mip05Error::TokenRemovalMustNotContainTags);
    }

    Ok(TokenRemoval::default())
}

fn parse_token_removal_app_event(event: &UnsignedEvent) -> Result<TokenRemoval, Mip05Error> {
    validate_version_tag(event)?;
    let payload = parse_removal_payload(&event.content)?;
    if payload.removals.is_empty() {
        return Err(Mip05Error::MissingSourceMember);
    }

    let mut member_pubkeys = Vec::with_capacity(payload.removals.len());
    for removal in payload.removals {
        NotificationPlatform::from_platform_str(&removal.platform)?;
        validate_token_fingerprint(&removal.token_fingerprint)?;
        parse_public_key_hex(&removal.server_pubkey_hex)?;
        member_pubkeys.push(parse_public_key_hex(&removal.member_id_hex)?);
    }

    Ok(TokenRemoval { member_pubkeys })
}

impl PushTokenGossipEntry {
    fn from_member_token(token: MemberTokenTag) -> Result<Self, Mip05Error> {
        let platform = token
            .token_tag
            .platform
            .ok_or(Mip05Error::MissingTokenPlatform)?;
        let token_fingerprint = token
            .token_tag
            .token_fingerprint
            .clone()
            .ok_or(Mip05Error::MissingTokenFingerprint)?;
        validate_token_fingerprint(&token_fingerprint)?;

        Ok(Self::from_token(
            token.member_pubkey,
            token.leaf_index,
            platform,
            &token_fingerprint,
            token.token_tag,
        ))
    }

    fn from_token(
        member_pubkey: PublicKey,
        leaf_index: u32,
        platform: NotificationPlatform,
        token_fingerprint: &str,
        token: TokenTag,
    ) -> Self {
        Self {
            member_id_hex: member_pubkey.to_hex(),
            leaf_index,
            platform: platform.as_str().to_string(),
            token_fingerprint: token_fingerprint.to_string(),
            server_pubkey_hex: token.server_pubkey.to_hex(),
            relay_hint: Some(token.relay_hint.to_string()),
            encrypted_token: token.encrypted_token.to_base64(),
        }
    }

    fn into_token_tag(self) -> Result<TokenTag, Mip05Error> {
        let platform = NotificationPlatform::from_platform_str(&self.platform)?;
        validate_token_fingerprint(&self.token_fingerprint)?;
        parse_public_key_hex(&self.member_id_hex)?;

        Ok(TokenTag {
            encrypted_token: EncryptedToken::from_base64(&self.encrypted_token)?,
            platform: Some(platform),
            token_fingerprint: Some(self.token_fingerprint),
            server_pubkey: parse_public_key_hex(&self.server_pubkey_hex)?,
            relay_hint: self
                .relay_hint
                .as_ref()
                .ok_or(Mip05Error::InvalidNotificationRelayHint)
                .and_then(|relay| {
                    RelayUrl::parse(relay).map_err(|_| Mip05Error::InvalidNotificationRelayHint)
                })?,
        })
    }

    fn into_leaf_token_tag(self) -> Result<LeafTokenTag, Mip05Error> {
        let leaf_index = self.leaf_index;
        Ok(LeafTokenTag {
            token_tag: self.into_token_tag()?,
            leaf_index,
        })
    }
}

fn parse_gossip_payload(content: &str) -> Result<PushTokenGossipPayload, Mip05Error> {
    let payload: PushTokenGossipPayload =
        serde_json::from_str(content).map_err(|_| Mip05Error::MalformedJsonPayload)?;
    if payload.v != MIP05_VERSION {
        return Err(Mip05Error::UnsupportedVersion);
    }
    Ok(payload)
}

fn parse_removal_payload(content: &str) -> Result<PushTokenRemovalPayload, Mip05Error> {
    let payload: PushTokenRemovalPayload =
        serde_json::from_str(content).map_err(|_| Mip05Error::MalformedJsonPayload)?;
    if payload.v != MIP05_VERSION {
        return Err(Mip05Error::UnsupportedVersion);
    }
    Ok(payload)
}

fn source_from_gossip_entries(entries: &[PushTokenGossipEntry]) -> Result<TokenSource, Mip05Error> {
    let first = entries.first().ok_or(Mip05Error::MissingSourceMember)?;
    let source = TokenSource {
        member_pubkey: parse_public_key_hex(&first.member_id_hex)?,
        leaf_index: first.leaf_index,
    };

    if entries.iter().all(|entry| {
        entry.member_id_hex == first.member_id_hex && entry.leaf_index == first.leaf_index
    }) {
        Ok(source)
    } else {
        Err(Mip05Error::InvalidTokenTagShape)
    }
}

fn build_token_request_tags(tokens: Vec<TokenTag>) -> Vec<Tag> {
    tokens.into_iter().map(build_token_tag).collect()
}

fn build_token_list_response_tags(tokens: Vec<LeafTokenTag>) -> Vec<Tag> {
    tokens.into_iter().map(build_leaf_token_tag).collect()
}

fn build_token_tag(token: TokenTag) -> Tag {
    Tag::custom(
        TagKind::Custom(TOKEN_TAG_NAME.into()),
        [
            token.encrypted_token.to_base64(),
            token.server_pubkey.to_hex(),
            token.relay_hint.to_string(),
        ],
    )
}

fn build_leaf_token_tag(token: LeafTokenTag) -> Tag {
    Tag::custom(
        TagKind::Custom(TOKEN_TAG_NAME.into()),
        [
            token.token_tag.encrypted_token.to_base64(),
            token.token_tag.server_pubkey.to_hex(),
            token.token_tag.relay_hint.to_string(),
            token.leaf_index.to_string(),
        ],
    )
}

fn parse_token_tag(tag: &Tag) -> Result<TokenTag, Mip05Error> {
    let values = tag.as_slice();
    if values.len() != 4 {
        return Err(Mip05Error::InvalidTokenTagShape);
    }

    Ok(TokenTag {
        encrypted_token: EncryptedToken::from_base64(&values[1])?,
        platform: None,
        token_fingerprint: None,
        server_pubkey: PublicKey::parse(&values[2])
            .map_err(|_| Mip05Error::InvalidNotificationServerPublicKey)?,
        relay_hint: RelayUrl::parse(&values[3])
            .map_err(|_| Mip05Error::InvalidNotificationRelayHint)?,
    })
}

fn parse_leaf_token_tag(tag: &Tag) -> Result<LeafTokenTag, Mip05Error> {
    let values = tag.as_slice();
    if values.len() != 5 {
        return Err(Mip05Error::InvalidTokenTagShape);
    }

    let token_tag = TokenTag {
        encrypted_token: EncryptedToken::from_base64(&values[1])?,
        platform: None,
        token_fingerprint: None,
        server_pubkey: PublicKey::parse(&values[2])
            .map_err(|_| Mip05Error::InvalidNotificationServerPublicKey)?,
        relay_hint: RelayUrl::parse(&values[3])
            .map_err(|_| Mip05Error::InvalidNotificationRelayHint)?,
    };
    let leaf_index = values[4]
        .parse::<u32>()
        .map_err(|_| Mip05Error::InvalidLeafIndex)?;

    Ok(LeafTokenTag {
        token_tag,
        leaf_index,
    })
}

fn parse_event_reference(tag: &Tag) -> Result<EventId, Mip05Error> {
    let event_id = tag.content().ok_or(Mip05Error::MissingEventReference)?;
    EventId::from_hex(event_id).map_err(|_| Mip05Error::InvalidEventReference)
}

fn validate_empty_content(event: &UnsignedEvent) -> Result<(), Mip05Error> {
    if !event.content.is_empty() {
        return Err(Mip05Error::NonEmptyContent);
    }
    Ok(())
}

fn validate_version_tag(event: &UnsignedEvent) -> Result<(), Mip05Error> {
    if event.tags.iter().any(|tag| tag == &version_tag()) {
        Ok(())
    } else {
        Err(Mip05Error::UnsupportedVersion)
    }
}

fn validate_unique_leaf_indices(tokens: &[LeafTokenTag]) -> Result<(), Mip05Error> {
    let unique_leaf_indices: BTreeSet<u32> = tokens.iter().map(|token| token.leaf_index).collect();
    if unique_leaf_indices.len() != tokens.len() {
        return Err(Mip05Error::DuplicateLeafIndex);
    }
    Ok(())
}

fn validate_unique_member_token_leaf_indices(tokens: &[MemberTokenTag]) -> Result<(), Mip05Error> {
    let unique_leaf_indices: BTreeSet<u32> = tokens.iter().map(|token| token.leaf_index).collect();
    if unique_leaf_indices.len() != tokens.len() {
        return Err(Mip05Error::DuplicateLeafIndex);
    }
    Ok(())
}

fn validate_unique_gossip_leaf_indices(tokens: &[PushTokenGossipEntry]) -> Result<(), Mip05Error> {
    let unique_leaf_indices: BTreeSet<u32> = tokens.iter().map(|token| token.leaf_index).collect();
    if unique_leaf_indices.len() != tokens.len() {
        return Err(Mip05Error::DuplicateLeafIndex);
    }
    Ok(())
}

fn validate_token_fingerprint(value: &str) -> Result<(), Mip05Error> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(Mip05Error::InvalidTokenFingerprint);
    };
    if hex.len() == 24 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(Mip05Error::InvalidTokenFingerprint)
    }
}

fn parse_public_key_hex(value: &str) -> Result<PublicKey, Mip05Error> {
    PublicKey::from_hex(value).map_err(|_| Mip05Error::InvalidMemberPublicKey)
}

fn version_tag() -> Tag {
    Tag::custom(TagKind::Custom(VERSION_TAG_NAME.into()), [MIP05_VERSION])
}

fn encrypt_push_token_with_materials(
    server_pubkey: &PublicKey,
    plaintext: &PushTokenPlaintext,
    ephemeral_secret_key: SecretKey,
    nonce_bytes: [u8; NONCE_LEN],
    padding: &[u8],
) -> Result<EncryptedToken, Mip05Error> {
    let ephemeral_keys = Keys::new(ephemeral_secret_key);
    let server_pubkey = secp_public_key_from_nostr_pubkey(server_pubkey)?;
    let key = derive_encryption_key(ephemeral_keys.secret_key(), &server_pubkey)?;
    let cipher = ChaCha20Poly1305::new_from_slice(key.as_slice())
        .map_err(|_| Mip05Error::KeyDerivationFailed)?;
    let padded_plaintext = Zeroizing::new(plaintext.encode_padded(padding)?);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: padded_plaintext.as_ref(),
                aad: b"",
            },
        )
        .map_err(|_| Mip05Error::EncryptionFailed)?;

    if ciphertext.len() != TOKEN_CIPHERTEXT_LEN {
        return Err(Mip05Error::InvalidCiphertextLength);
    }

    let mut bytes = [0u8; ENCRYPTED_TOKEN_LEN];
    bytes[..EPHEMERAL_PUBKEY_LEN].copy_from_slice(ephemeral_keys.public_key().as_bytes());
    bytes[EPHEMERAL_PUBKEY_LEN..CIPHERTEXT_OFFSET].copy_from_slice(&nonce_bytes);
    bytes[CIPHERTEXT_OFFSET..].copy_from_slice(&ciphertext);

    Ok(EncryptedToken::from(bytes))
}

fn derive_encryption_key(
    secret_key: &SecretKey,
    public_key: &SecpPublicKey,
) -> Result<Zeroizing<[u8; 32]>, Mip05Error> {
    let shared_point = ecdh::shared_secret_point(public_key, secret_key);
    let shared_x: [u8; 32] = shared_point[..32]
        .try_into()
        .map_err(|_| Mip05Error::KeyDerivationFailed)?;
    let hkdf = Hkdf::<Sha256>::new(Some(TOKEN_ENCRYPTION_SALT), &shared_x);
    let mut encryption_key = [0u8; 32];
    hkdf.expand(TOKEN_ENCRYPTION_INFO, &mut encryption_key)
        .map_err(|_| Mip05Error::KeyDerivationFailed)?;
    Ok(Zeroizing::new(encryption_key))
}

fn secp_public_key_from_nostr_pubkey(pubkey: &PublicKey) -> Result<SecpPublicKey, Mip05Error> {
    secp_public_key_from_xonly_bytes(pubkey.as_bytes())
}

fn secp_public_key_from_xonly_bytes(bytes: &[u8]) -> Result<SecpPublicKey, Mip05Error> {
    let xonly = XOnlyPublicKey::from_slice(bytes)
        .map_err(|_| Mip05Error::InvalidEncryptedTokenPublicKey)?;
    Ok(SecpPublicKey::from_x_only_public_key(xonly, Parity::Even))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_request_rumor_round_trips_without_content() {
        let sender = Keys::generate();
        let token = token_tag(1);

        let rumor = build_token_request_rumor(
            sender.public_key(),
            Timestamp::from(123),
            vec![token.clone()],
        )
        .unwrap();
        let parsed = parse_group_message_rumor(&rumor).unwrap();

        assert_eq!(rumor.kind, Kind::from(TOKEN_REQUEST_KIND));
        assert!(rumor.content.is_empty());
        assert_eq!(
            parsed,
            Mip05GroupMessage::TokenRequest(TokenRequest {
                source: None,
                tokens: vec![token]
            })
        );
    }

    #[test]
    fn token_update_app_event_round_trips_with_darkmatter_payload_shape() {
        let sender = Keys::generate();
        let fingerprint = push_token_fingerprint(NotificationPlatform::Fcm, b"firebase-token");
        let token = TokenTag {
            platform: Some(NotificationPlatform::Fcm),
            token_fingerprint: Some(fingerprint.clone()),
            ..token_tag(1)
        };

        let event = build_token_update_app_event(
            sender.public_key(),
            Timestamp::from(123),
            sender.public_key(),
            7,
            NotificationPlatform::Fcm,
            &fingerprint,
            token.clone(),
        )
        .unwrap();
        let parsed = parse_group_message_rumor(&event).unwrap();

        assert_eq!(event.kind, Kind::from(TOKEN_REQUEST_KIND));
        assert!(!event.content.is_empty());
        assert_eq!(event.tags.len(), 1);
        assert!(event.tags.iter().any(|tag| tag == &version_tag()));
        assert_eq!(
            parsed,
            Mip05GroupMessage::TokenRequest(TokenRequest {
                source: Some(TokenSource {
                    member_pubkey: sender.public_key(),
                    leaf_index: 7,
                }),
                tokens: vec![token],
            })
        );
    }

    #[test]
    fn token_list_response_app_event_round_trips_with_darkmatter_payload_shape() {
        let sender = Keys::generate();
        let member = Keys::generate();
        let fingerprint = push_token_fingerprint(NotificationPlatform::Fcm, b"firebase-token");
        let token = TokenTag {
            platform: Some(NotificationPlatform::Fcm),
            token_fingerprint: Some(fingerprint.clone()),
            ..token_tag(1)
        };

        let event = build_token_list_response_app_event(
            sender.public_key(),
            Timestamp::from(123),
            vec![MemberTokenTag {
                member_pubkey: member.public_key(),
                leaf_index: 7,
                token_tag: token.clone(),
            }],
        )
        .unwrap();
        let parsed = parse_group_message_rumor(&event).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&event.content).unwrap();

        assert_eq!(event.kind, Kind::from(TOKEN_LIST_RESPONSE_KIND));
        assert!(!event.content.is_empty());
        assert_eq!(event.tags.len(), 1);
        assert!(event.tags.iter().any(|tag| tag == &version_tag()));
        assert_eq!(payload["v"], MIP05_VERSION);
        assert_eq!(
            payload["tokens"][0]["member_id_hex"],
            member.public_key().to_hex()
        );
        assert_eq!(payload["tokens"][0]["leaf_index"], 7);
        assert_eq!(payload["tokens"][0]["platform"], "fcm");
        assert_eq!(payload["tokens"][0]["token_fingerprint"], fingerprint);
        assert_eq!(
            parsed,
            Mip05GroupMessage::TokenListResponse(TokenListResponse {
                request_event_id: None,
                tokens: vec![LeafTokenTag {
                    token_tag: token,
                    leaf_index: 7,
                }],
            })
        );
    }

    #[test]
    fn token_list_response_rejects_duplicate_leaf_indices() {
        let sender = Keys::generate();
        let request_id = EventId::all_zeros();
        let duplicate_tokens = vec![
            LeafTokenTag {
                token_tag: token_tag(1),
                leaf_index: 7,
            },
            LeafTokenTag {
                token_tag: token_tag(2),
                leaf_index: 7,
            },
        ];

        let error = build_token_list_response_rumor(
            sender.public_key(),
            Timestamp::from(123),
            request_id,
            duplicate_tokens,
        )
        .unwrap_err();

        assert_eq!(error, Mip05Error::DuplicateLeafIndex);
    }

    #[test]
    fn token_removal_rumor_round_trips_with_no_tags() {
        let sender = Keys::generate();

        let rumor = build_token_removal_rumor(sender.public_key(), Timestamp::from(123));
        let parsed = parse_group_message_rumor(&rumor).unwrap();

        assert_eq!(rumor.kind, Kind::from(TOKEN_REMOVAL_KIND));
        assert!(rumor.content.is_empty());
        assert!(rumor.tags.is_empty());
        assert_eq!(
            parsed,
            Mip05GroupMessage::TokenRemoval(TokenRemoval::default())
        );
    }

    #[test]
    fn encrypted_push_token_round_trips_with_darkmatter_wire_layout() {
        let server_keys = Keys::generate();
        let plaintext =
            PushTokenPlaintext::new(NotificationPlatform::Fcm, b"firebase-token".to_vec()).unwrap();

        let encrypted = encrypt_push_token(&server_keys.public_key(), &plaintext).unwrap();
        let decrypted = decrypt_push_token(server_keys.secret_key(), &encrypted).unwrap();

        assert_eq!(decrypted, plaintext);
        assert_eq!(encrypted.as_bytes().len(), ENCRYPTED_TOKEN_LEN);
    }

    #[test]
    fn encryption_key_uses_raw_shared_x_coordinate() {
        let server_keys = Keys::generate();
        let peer_keys = Keys::generate();
        let peer_public_key = secp_public_key_from_nostr_pubkey(&peer_keys.public_key()).unwrap();

        let derived = derive_encryption_key(server_keys.secret_key(), &peer_public_key).unwrap();
        let shared_point = ecdh::shared_secret_point(&peer_public_key, server_keys.secret_key());
        let mut expected = [0u8; 32];
        Hkdf::<Sha256>::new(Some(TOKEN_ENCRYPTION_SALT), &shared_point[..32])
            .expand(TOKEN_ENCRYPTION_INFO, &mut expected)
            .unwrap();

        assert_eq!(derived.as_slice(), expected);
    }

    fn token_tag(seed: u8) -> TokenTag {
        TokenTag {
            encrypted_token: EncryptedToken::from([seed; ENCRYPTED_TOKEN_LEN]),
            platform: None,
            token_fingerprint: None,
            server_pubkey: Keys::generate().public_key(),
            relay_hint: RelayUrl::parse("wss://push.example.com").unwrap(),
        }
    }
}

use cgka_engine::openmls_projection::{OpenMlsContentKind, project_mls_message};
use cgka_traits::group::Group;
use cgka_traits::message::{MessageRecord, MessageState, StoredMessagePayload};
use cgka_traits::transport::TransportEnvelope;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupForensics {
    pub group_id: String,
    pub epoch: u64,
    pub member_count: u32,
    #[serde(default)]
    pub required_app_components: Vec<u16>,
    pub messages: Vec<GroupForensicsMessage>,
    pub snapshots: Vec<GroupForensicsSnapshot>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupForensicsMessage {
    pub message_id: String,
    pub group_id: String,
    pub epoch: u64,
    pub state: String,
    pub payload_kind: String,
    pub envelope_kind: String,
    pub timestamp: u64,
    pub payload_len: u64,
    pub payload_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openmls: Option<GroupForensicsOpenMlsMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupForensicsOpenMlsMessage {
    pub content_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_epoch: Option<u64>,
    pub message_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupForensicsSnapshot {
    pub name: String,
}

impl GroupForensics {
    pub(crate) fn from_storage(
        redaction_salt: &[u8],
        group: Group,
        records: Vec<MessageRecord>,
        snapshots: Vec<String>,
    ) -> Self {
        let redactor = ForensicsRedactor::new(redaction_salt);
        let mut warnings = Vec::new();
        let mut messages = Vec::new();

        for record in records {
            match forensic_message_from_record(&redactor, &record) {
                Ok(message) => messages.push(message),
                Err(warning) => warnings.push(warning),
            }
        }

        Self {
            group_id: redactor.protect_hex(&hex::encode(group.id.as_slice())),
            epoch: group.epoch.0,
            member_count: group.members.len() as u32,
            required_app_components: group
                .required_capabilities
                .app_components
                .ids
                .iter()
                .copied()
                .collect(),
            messages,
            snapshots: snapshots
                .into_iter()
                .map(|name| GroupForensicsSnapshot {
                    name: redactor.protect_text(&name),
                })
                .collect(),
            warnings,
        }
    }
}

struct ForensicsRedactor<'a> {
    salt: &'a [u8],
}

impl<'a> ForensicsRedactor<'a> {
    fn new(salt: &'a [u8]) -> Self {
        debug_assert!(
            !salt.is_empty(),
            "public group forensics require a redaction salt"
        );
        Self { salt }
    }

    fn protect_hex(&self, value_hex: &str) -> String {
        let decoded = hex::decode(value_hex).unwrap_or_else(|_| value_hex.as_bytes().to_vec());
        format!("hash:{}", self.salted_hash_hex(&decoded))
    }

    fn protect_text(&self, value: &str) -> String {
        format!("hash:{}", self.salted_hash_hex(value.as_bytes()))
    }

    fn protect_digest_hex(&self, digest_hex: &str) -> String {
        format!(
            "salted_sha256:{}",
            self.salted_hash_hex(digest_hex.as_bytes())
        )
    }

    fn capture_payload(&self, bytes: &[u8]) -> (u64, String, Option<String>) {
        let digest = Sha256::digest(bytes);
        let payload_digest = self.protect_digest_hex(&hex::encode(digest));
        (bytes.len() as u64, payload_digest, None)
    }

    fn salted_hash_hex(&self, bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.salt);
        hasher.update(bytes);
        hex::encode(hasher.finalize())
    }
}

fn forensic_message_from_record(
    redactor: &ForensicsRedactor<'_>,
    record: &MessageRecord,
) -> Result<GroupForensicsMessage, String> {
    let stored_payload = StoredMessagePayload::decode(&record.payload)
        .map_err(|error| format!("message payload decode failed: {error}"))?;
    let (payload_kind, message) = match stored_payload {
        StoredMessagePayload::RawTransport(message) => ("raw_transport", message),
        StoredMessagePayload::OpenMlsWire(message) => ("openmls_wire", message),
    };
    let (payload_len, payload_digest, payload_hex) = redactor.capture_payload(&message.payload);
    let openmls = (payload_kind == "openmls_wire")
        .then(|| openmls_forensics(redactor, &message.payload))
        .transpose()?;

    Ok(GroupForensicsMessage {
        message_id: redactor.protect_hex(&hex::encode(record.id.as_slice())),
        group_id: redactor.protect_hex(&hex::encode(record.group_id.as_slice())),
        epoch: record.epoch.0,
        state: message_state_name(record.state).to_owned(),
        payload_kind: payload_kind.to_owned(),
        envelope_kind: envelope_kind_name(&message.envelope).to_owned(),
        timestamp: message.timestamp.0,
        payload_len,
        payload_digest,
        payload_hex,
        openmls,
    })
}

fn openmls_forensics(
    redactor: &ForensicsRedactor<'_>,
    payload: &[u8],
) -> Result<GroupForensicsOpenMlsMessage, String> {
    let projection = project_mls_message(payload)
        .map_err(|error| format!("OpenMLS projection failed: {error}"))?;
    let message_digest = redactor.protect_digest_hex(&hex::encode(projection.message_digest));

    Ok(GroupForensicsOpenMlsMessage {
        content_kind: openmls_content_kind_name(projection.kind).to_owned(),
        source_epoch: projection.source_epoch,
        message_digest,
    })
}

fn message_state_name(state: MessageState) -> &'static str {
    match state {
        MessageState::Sent => "sent",
        MessageState::Created => "created",
        MessageState::Processed => "processed",
        MessageState::Failed => "failed",
        MessageState::Retryable => "retryable",
        MessageState::PeelDeferred => "peel_deferred",
        MessageState::EpochInvalidated => "epoch_invalidated",
    }
}

fn envelope_kind_name(envelope: &TransportEnvelope) -> &'static str {
    match envelope {
        TransportEnvelope::GroupMessage { .. } => "group_message",
        TransportEnvelope::Welcome { .. } => "welcome",
    }
}

fn openmls_content_kind_name(kind: OpenMlsContentKind) -> &'static str {
    match kind {
        OpenMlsContentKind::Application => "application",
        OpenMlsContentKind::Proposal => "proposal",
        OpenMlsContentKind::Commit => "commit",
        OpenMlsContentKind::Welcome => "welcome",
        OpenMlsContentKind::Other => "other",
    }
}

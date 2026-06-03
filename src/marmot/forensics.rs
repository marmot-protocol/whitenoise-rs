use serde::{Deserialize, Serialize};

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

impl From<marmot_forensics::ForensicsEngineGroupState> for GroupForensics {
    fn from(value: marmot_forensics::ForensicsEngineGroupState) -> Self {
        Self {
            group_id: value.group_id,
            epoch: value.epoch,
            member_count: value.member_count,
            required_app_components: value.required_app_components,
            messages: value.messages.into_iter().map(Into::into).collect(),
            snapshots: value.snapshots.into_iter().map(Into::into).collect(),
            warnings: value.warnings,
        }
    }
}

impl From<marmot_forensics::ForensicsMessage> for GroupForensicsMessage {
    fn from(value: marmot_forensics::ForensicsMessage) -> Self {
        Self {
            message_id: value.message_id,
            group_id: value.group_id,
            epoch: value.epoch,
            state: value.state,
            payload_kind: value.payload_kind,
            envelope_kind: value.envelope_kind,
            timestamp: value.timestamp,
            payload_len: value.payload_len,
            payload_digest: value.payload_digest,
            payload_hex: value.payload_hex,
            openmls: value.openmls.map(Into::into),
        }
    }
}

impl From<marmot_forensics::ForensicsOpenMlsMessage> for GroupForensicsOpenMlsMessage {
    fn from(value: marmot_forensics::ForensicsOpenMlsMessage) -> Self {
        Self {
            content_kind: value.content_kind,
            source_epoch: value.source_epoch,
            message_digest: value.message_digest,
        }
    }
}

impl From<marmot_forensics::ForensicsSnapshot> for GroupForensicsSnapshot {
    fn from(value: marmot_forensics::ForensicsSnapshot) -> Self {
        Self { name: value.name }
    }
}

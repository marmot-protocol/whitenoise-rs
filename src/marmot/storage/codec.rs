use cgka_traits::message::MessageState;
use cgka_traits::storage::{StorageError, StorageResult};
use cgka_traits::types::EpochId;

pub(super) fn serialize<T>(value: &T) -> StorageResult<Vec<u8>>
where
    T: serde::Serialize,
{
    serde_json::to_vec(value).map_err(|err| StorageError::Serialization(err.to_string()))
}

pub(super) fn deserialize<T>(record: &[u8]) -> StorageResult<T>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_slice(record).map_err(|err| StorageError::Serialization(err.to_string()))
}

pub(super) fn epoch_to_i64(epoch: EpochId) -> StorageResult<i64> {
    i64::try_from(epoch.0)
        .map_err(|_| StorageError::Serialization(format!("epoch too large: {}", epoch.0)))
}

pub(super) fn created_at_to_i64(created_at_ms: u64) -> StorageResult<i64> {
    i64::try_from(created_at_ms).map_err(|_| {
        StorageError::Serialization(format!("created_at_ms too large: {created_at_ms}"))
    })
}

pub(super) fn message_state_to_i64(state: MessageState) -> i64 {
    match state {
        MessageState::Sent => 0,
        MessageState::Created => 1,
        MessageState::Processed => 2,
        MessageState::Failed => 3,
        MessageState::Retryable => 4,
        MessageState::EpochInvalidated => 5,
        MessageState::PeelDeferred => 6,
    }
}

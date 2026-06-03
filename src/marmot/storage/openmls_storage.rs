mod labels;
mod provider;
mod value_store;

use cgka_traits::storage::{StorageError, StorageResult};
use cgka_traits::types::GroupId as MarmotGroupId;
use serde::Serialize;

use super::SharedConnection;

#[derive(Clone, Debug)]
pub(crate) struct WhitenoiseOpenMlsStorage {
    connection: SharedConnection,
}

impl WhitenoiseOpenMlsStorage {
    pub(super) fn new(connection: SharedConnection) -> Self {
        Self { connection }
    }

    pub(super) fn group_key<GroupId>(group_id: &GroupId) -> Result<Vec<u8>, OpenMlsStorageError>
    where
        GroupId: Serialize,
    {
        Ok(serde_json::to_vec(group_id)?)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub(super) fn mls_group_key(group_id: &MarmotGroupId) -> StorageResult<Vec<u8>> {
    let mls_group_id = openmls::group::GroupId::from_slice(group_id.as_slice());
    WhitenoiseOpenMlsStorage::group_key(&mls_group_id)
        .map_err(|err| StorageError::Serialization(err.to_string()))
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum OpenMlsStorageError {
    #[error("sqlite failure: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization failure: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("queued proposal reference was present without a queued proposal")]
    MissingQueuedProposal,
}

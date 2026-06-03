use cgka_traits::group::Group;
use cgka_traits::storage::{GroupStorage, StorageError, StorageResult};
use cgka_traits::types::GroupId;
use rusqlite::{OptionalExtension, params};

use crate::marmot::MarmotCreatedGroupProjection;

use super::WhitenoiseMarmotStorage;
use super::codec::{deserialize, epoch_to_i64, serialize};
use super::openmls_storage::mls_group_key;
use super::result_ext::RusqliteResultExt;

const GROUP_SCOPED_TABLES: &[&str] = &[
    "marmot_messages",
    "marmot_queued_outbound",
    "marmot_welcomes",
    "marmot_member_capabilities",
    "marmot_convergence_policies",
    "marmot_group_snapshots",
    "marmot_group_projections",
];

impl WhitenoiseMarmotStorage {
    pub(crate) fn put_group_projection(
        &self,
        projection: &MarmotCreatedGroupProjection,
    ) -> StorageResult<()> {
        self.lock()
            .execute(
                "INSERT INTO marmot_group_projections (group_id, record)
                 VALUES (?1, ?2)
                 ON CONFLICT(group_id) DO UPDATE SET
                    record = excluded.record",
                params![projection.group_id.as_slice(), serialize(projection)?],
            )
            .storage()?;

        Ok(())
    }

    pub(crate) fn find_group_projection(
        &self,
        group_id: &GroupId,
    ) -> StorageResult<Option<MarmotCreatedGroupProjection>> {
        let record = self
            .lock()
            .query_row(
                "SELECT record FROM marmot_group_projections WHERE group_id = ?1",
                params![group_id.as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .storage()?;

        match record {
            Some(record) => deserialize(&record).map(Some),
            None => Ok(None),
        }
    }

    pub(crate) fn list_group_projections(
        &self,
    ) -> StorageResult<Vec<MarmotCreatedGroupProjection>> {
        let connection = self.lock();
        let mut statement = connection
            .prepare("SELECT record FROM marmot_group_projections ORDER BY group_id")
            .storage()?;

        statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .storage()?
            .map(|row| row.storage().and_then(|record| deserialize(&record)))
            .collect()
    }

    pub(crate) fn delete_group_state_if_exists(&self, id: &GroupId) -> StorageResult<bool> {
        self.delete_group_state(id, false)
    }

    fn delete_group_state(&self, id: &GroupId, require_group: bool) -> StorageResult<bool> {
        let openmls_group_key = mls_group_key(id)?;
        let mut connection = self.lock();
        let transaction = connection.transaction().storage()?;
        let mut deleted_any = transaction
            .execute(
                "DELETE FROM marmot_groups WHERE id = ?1",
                params![id.as_slice()],
            )
            .storage()?
            > 0;

        if require_group && !deleted_any {
            return Err(StorageError::NotFound);
        }

        for table in GROUP_SCOPED_TABLES {
            deleted_any |= transaction
                .execute(
                    &format!("DELETE FROM {table} WHERE group_id = ?1"),
                    params![id.as_slice()],
                )
                .storage()?
                > 0;
        }
        deleted_any |= transaction
            .execute(
                "DELETE FROM marmot_openmls_values
                 WHERE provider_version = ?1 AND group_key = ?2",
                params![openmls_traits::storage::CURRENT_VERSION, openmls_group_key],
            )
            .storage()?
            > 0;

        transaction.commit().storage()?;

        Ok(deleted_any)
    }
}

impl GroupStorage for WhitenoiseMarmotStorage {
    fn put_group(&self, group: &Group) -> StorageResult<()> {
        self.lock()
            .execute(
                "INSERT INTO marmot_groups (id, epoch, record)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET
                    epoch = excluded.epoch,
                    record = excluded.record",
                params![
                    group.id.as_slice(),
                    epoch_to_i64(group.epoch)?,
                    serialize(group)?
                ],
            )
            .storage()?;

        Ok(())
    }

    fn get_group(&self, id: &GroupId) -> StorageResult<Group> {
        let record: Vec<u8> = self
            .lock()
            .query_row(
                "SELECT record FROM marmot_groups WHERE id = ?1",
                params![id.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .storage()?
            .ok_or(StorageError::NotFound)?;

        deserialize(&record)
    }

    fn delete_group(&self, id: &GroupId) -> StorageResult<()> {
        self.delete_group_state(id, true).map(|_| ())
    }

    fn list_groups(&self) -> StorageResult<Vec<GroupId>> {
        let connection = self.lock();
        let mut statement = connection
            .prepare("SELECT id FROM marmot_groups ORDER BY id")
            .storage()?;

        statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0).map(GroupId::new))
            .storage()?
            .collect::<Result<Vec<_>, _>>()
            .storage()
    }
}

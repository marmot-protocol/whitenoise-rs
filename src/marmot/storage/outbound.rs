use cgka_traits::storage::{
    OutboundIntentStorage, QueuedOutboundIntent, StorageError, StorageResult,
};
use cgka_traits::types::{GroupId, MessageId};
use rusqlite::params;

use super::WhitenoiseMarmotStorage;
use super::codec::{created_at_to_i64, deserialize, serialize};
use super::result_ext::RusqliteResultExt;

impl OutboundIntentStorage for WhitenoiseMarmotStorage {
    fn put_queued_outbound_intent(&self, record: &QueuedOutboundIntent) -> StorageResult<()> {
        self.lock()
            .execute(
                "INSERT INTO marmot_queued_outbound (id, group_id, created_at_ms, record)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                    group_id = excluded.group_id,
                    created_at_ms = excluded.created_at_ms,
                    record = excluded.record",
                params![
                    record.id.as_slice(),
                    record.group_id.as_slice(),
                    created_at_to_i64(record.created_at_ms)?,
                    serialize(record)?
                ],
            )
            .storage()?;

        Ok(())
    }

    fn list_queued_outbound_intents(
        &self,
        group_id: &GroupId,
    ) -> StorageResult<Vec<QueuedOutboundIntent>> {
        let connection = self.lock();
        let mut statement = connection
            .prepare(
                "SELECT record FROM marmot_queued_outbound
                 WHERE group_id = ?1
                 ORDER BY insert_order",
            )
            .storage()?;
        let records = statement
            .query_map(params![group_id.as_slice()], |row| row.get::<_, Vec<u8>>(0))
            .storage()?
            .collect::<Result<Vec<_>, _>>()
            .storage()?;

        records.iter().map(|record| deserialize(record)).collect()
    }

    fn delete_queued_outbound_intent(&self, id: &MessageId) -> StorageResult<()> {
        let changed = self
            .lock()
            .execute(
                "DELETE FROM marmot_queued_outbound WHERE id = ?1",
                params![id.as_slice()],
            )
            .storage()?;
        if changed == 0 {
            return Err(StorageError::NotFound);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use cgka_traits::storage::{OutboundIntentStorage, StorageError};

    use super::super::WhitenoiseMarmotStorage;
    use super::super::test_support::{gid, mid, sample_queued_intent};

    #[test]
    fn queued_outbound_intents_are_group_scoped_and_ordered() {
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        storage
            .put_queued_outbound_intent(&sample_queued_intent(mid(3), gid(1)))
            .unwrap();
        storage
            .put_queued_outbound_intent(&sample_queued_intent(mid(1), gid(1)))
            .unwrap();
        storage
            .put_queued_outbound_intent(&sample_queued_intent(mid(2), gid(2)))
            .unwrap();

        let ids: Vec<_> = storage
            .list_queued_outbound_intents(&gid(1))
            .unwrap()
            .into_iter()
            .map(|queued| queued.id)
            .collect();
        assert_eq!(ids, vec![mid(3), mid(1)]);

        storage.delete_queued_outbound_intent(&mid(3)).unwrap();
        let ids: Vec<_> = storage
            .list_queued_outbound_intents(&gid(1))
            .unwrap()
            .into_iter()
            .map(|queued| queued.id)
            .collect();
        assert_eq!(ids, vec![mid(1)]);
    }

    #[test]
    fn deleting_missing_queued_outbound_intent_returns_not_found() {
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();

        assert!(matches!(
            storage.delete_queued_outbound_intent(&mid(1)),
            Err(StorageError::NotFound)
        ));
    }
}

use cgka_traits::capabilities::GroupCapabilities;
use cgka_traits::group::Group;
use cgka_traits::message::{MessageRecord, MessageState};
use cgka_traits::storage::{MessageStorage, QueuedOutboundIntent, StorageError, StorageResult};
use cgka_traits::types::{EpochId, GroupId, MemberId, MessageId};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::WhitenoiseMarmotStorage;
use super::codec::{created_at_to_i64, deserialize, epoch_to_i64, message_state_to_i64, serialize};
use super::openmls_storage::mls_group_key;
use super::result_ext::RusqliteResultExt;

impl MessageStorage for WhitenoiseMarmotStorage {
    fn put_message(&self, record: &MessageRecord) -> StorageResult<()> {
        self.lock()
            .execute(
                "INSERT INTO marmot_messages (id, group_id, epoch, state, record)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET
                    group_id = excluded.group_id,
                    epoch = excluded.epoch,
                    state = excluded.state,
                    record = excluded.record",
                params![
                    record.id.as_slice(),
                    record.group_id.as_slice(),
                    epoch_to_i64(record.epoch)?,
                    message_state_to_i64(record.state),
                    serialize(record)?
                ],
            )
            .storage()?;

        Ok(())
    }

    fn get_message(&self, id: &MessageId) -> StorageResult<MessageRecord> {
        let record: Vec<u8> = self
            .lock()
            .query_row(
                "SELECT record FROM marmot_messages WHERE id = ?1",
                params![id.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .storage()?
            .ok_or(StorageError::NotFound)?;

        deserialize(&record)
    }

    fn update_message_state(&self, id: &MessageId, new_state: MessageState) -> StorageResult<()> {
        let mut connection = self.lock();
        let transaction = connection.transaction().storage()?;
        let record_bytes: Vec<u8> = transaction
            .query_row(
                "SELECT record FROM marmot_messages WHERE id = ?1",
                params![id.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .storage()?
            .ok_or(StorageError::NotFound)?;
        let mut record: MessageRecord = deserialize(&record_bytes)?;
        record.state = new_state;
        let changed = transaction
            .execute(
                "UPDATE marmot_messages SET state = ?1, record = ?2 WHERE id = ?3",
                params![
                    message_state_to_i64(new_state),
                    serialize(&record)?,
                    id.as_slice()
                ],
            )
            .storage()?;
        if changed == 0 {
            return Err(StorageError::NotFound);
        }
        transaction.commit().storage()
    }

    fn list_messages(
        &self,
        group_id: &GroupId,
        at_or_after_epoch: EpochId,
    ) -> StorageResult<Vec<MessageRecord>> {
        let connection = self.lock();
        let mut statement = connection
            .prepare(
                "SELECT record FROM marmot_messages
                 WHERE group_id = ?1 AND epoch >= ?2
                 ORDER BY insert_order",
            )
            .storage()?;
        let records = statement
            .query_map(
                params![group_id.as_slice(), epoch_to_i64(at_or_after_epoch)?],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .storage()?
            .collect::<Result<Vec<_>, _>>()
            .storage()?;

        records.iter().map(|record| deserialize(record)).collect()
    }

    fn create_group_snapshot(&self, group_id: &GroupId, name: &str) -> StorageResult<()> {
        create_group_snapshot(self, group_id, name)
    }

    fn list_group_snapshots(&self, group_id: &GroupId) -> StorageResult<Vec<String>> {
        let connection = self.lock();
        let mut statement = connection
            .prepare(
                "SELECT name FROM marmot_group_snapshots
                 WHERE group_id = ?1
                 ORDER BY name",
            )
            .storage()?;
        statement
            .query_map(params![group_id.as_slice()], |row| row.get(0))
            .storage()?
            .collect::<Result<Vec<_>, _>>()
            .storage()
    }

    fn rollback_group_to_snapshot(&self, group_id: &GroupId, name: &str) -> StorageResult<()> {
        rollback_group_to_snapshot(self, group_id, name)
    }

    fn release_group_snapshot(&self, group_id: &GroupId, name: &str) -> StorageResult<()> {
        let changed = self
            .lock()
            .execute(
                "DELETE FROM marmot_group_snapshots
                 WHERE group_id = ?1 AND name = ?2",
                params![group_id.as_slice(), name],
            )
            .storage()?;
        if changed == 0 {
            return Err(StorageError::SnapshotMissing(name.to_string()));
        }

        Ok(())
    }
}

fn create_group_snapshot(
    storage: &WhitenoiseMarmotStorage,
    group_id: &GroupId,
    name: &str,
) -> StorageResult<()> {
    let mls_group_key = mls_group_key(group_id)?;
    let mut connection = storage.lock();
    let transaction = connection.transaction().storage()?;
    let group_blob: Vec<u8> = transaction
        .query_row(
            "SELECT record FROM marmot_groups WHERE id = ?1",
            params![group_id.as_slice()],
            |row| row.get(0),
        )
        .optional()
        .storage()?
        .ok_or(StorageError::NotFound)?;
    let snapshot = Snapshot {
        group: deserialize(&group_blob)?,
        messages: snapshot_messages(&transaction, group_id)?,
        queued_outbound: snapshot_queued_outbound(&transaction, group_id)?,
        member_caps: snapshot_member_capabilities(&transaction, group_id)?,
        convergence_policy: snapshot_convergence_policy(&transaction, group_id)?,
        openmls_values: snapshot_openmls_values(&transaction, &mls_group_key)?,
    };

    transaction
        .execute(
            "INSERT OR REPLACE INTO marmot_group_snapshots (group_id, name, snapshot)
             VALUES (?1, ?2, ?3)",
            params![group_id.as_slice(), name, serialize(&snapshot)?],
        )
        .storage()?;
    transaction.commit().storage()
}

fn rollback_group_to_snapshot(
    storage: &WhitenoiseMarmotStorage,
    group_id: &GroupId,
    name: &str,
) -> StorageResult<()> {
    let mls_group_key = mls_group_key(group_id)?;
    let mut connection = storage.lock();
    let transaction = connection.transaction().storage()?;
    let snapshot_blob: Vec<u8> = transaction
        .query_row(
            "SELECT snapshot FROM marmot_group_snapshots
             WHERE group_id = ?1 AND name = ?2",
            params![group_id.as_slice(), name],
            |row| row.get(0),
        )
        .optional()
        .storage()?
        .ok_or_else(|| StorageError::SnapshotMissing(name.to_string()))?;
    let snapshot: Snapshot = deserialize(&snapshot_blob)?;

    restore_group(&transaction, group_id, &snapshot.group)?;
    restore_messages(&transaction, group_id, &snapshot.messages)?;
    restore_queued_outbound(&transaction, group_id, &snapshot.queued_outbound)?;
    restore_member_capabilities(&transaction, group_id, &snapshot.member_caps)?;
    restore_convergence_policy(
        &transaction,
        group_id,
        snapshot.convergence_policy.as_deref(),
    )?;
    restore_openmls_values(&transaction, &mls_group_key, &snapshot.openmls_values)?;

    transaction.commit().storage()
}

fn snapshot_messages(
    transaction: &rusqlite::Transaction<'_>,
    group_id: &GroupId,
) -> StorageResult<Vec<OrderedMessage>> {
    let mut statement = transaction
        .prepare(
            "SELECT insert_order, record FROM marmot_messages
             WHERE group_id = ?1
             ORDER BY insert_order",
        )
        .storage()?;
    let rows = statement
        .query_map(params![group_id.as_slice()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .storage()?
        .collect::<Result<Vec<_>, _>>()
        .storage()?;

    rows.into_iter()
        .map(|(insert_order, record)| {
            Ok(OrderedMessage {
                insert_order,
                record: deserialize(&record)?,
            })
        })
        .collect()
}

fn snapshot_queued_outbound(
    transaction: &rusqlite::Transaction<'_>,
    group_id: &GroupId,
) -> StorageResult<Vec<OrderedQueuedOutbound>> {
    let mut statement = transaction
        .prepare(
            "SELECT insert_order, record FROM marmot_queued_outbound
             WHERE group_id = ?1
             ORDER BY insert_order",
        )
        .storage()?;
    let rows = statement
        .query_map(params![group_id.as_slice()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .storage()?
        .collect::<Result<Vec<_>, _>>()
        .storage()?;

    rows.into_iter()
        .map(|(insert_order, record)| {
            Ok(OrderedQueuedOutbound {
                insert_order,
                record: deserialize(&record)?,
            })
        })
        .collect()
}

fn snapshot_member_capabilities(
    transaction: &rusqlite::Transaction<'_>,
    group_id: &GroupId,
) -> StorageResult<Vec<MemberCapabilitiesSnapshot>> {
    let mut statement = transaction
        .prepare(
            "SELECT member_id, capabilities FROM marmot_member_capabilities
             WHERE group_id = ?1",
        )
        .storage()?;
    let rows = statement
        .query_map(params![group_id.as_slice()], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .storage()?
        .collect::<Result<Vec<_>, _>>()
        .storage()?;

    rows.into_iter()
        .map(|(member_id, capabilities)| {
            Ok(MemberCapabilitiesSnapshot {
                member_id: MemberId::new(member_id),
                capabilities: deserialize(&capabilities)?,
            })
        })
        .collect()
}

fn snapshot_convergence_policy(
    transaction: &rusqlite::Transaction<'_>,
    group_id: &GroupId,
) -> StorageResult<Option<Vec<u8>>> {
    transaction
        .query_row(
            "SELECT policy FROM marmot_convergence_policies WHERE group_id = ?1",
            params![group_id.as_slice()],
            |row| row.get(0),
        )
        .optional()
        .storage()
}

fn snapshot_openmls_values(
    transaction: &rusqlite::Transaction<'_>,
    mls_group_key: &[u8],
) -> StorageResult<Vec<OpenMlsValueSnapshot>> {
    let mut statement = transaction
        .prepare(
            "SELECT label, storage_key, group_key, value FROM marmot_openmls_values
             WHERE provider_version = ?1 AND group_key = ?2
             ORDER BY storage_key",
        )
        .storage()?;
    statement
        .query_map(
            params![openmls_traits::storage::CURRENT_VERSION, mls_group_key],
            |row| {
                Ok(OpenMlsValueSnapshot {
                    label: row.get(0)?,
                    storage_key: row.get(1)?,
                    group_key: row.get::<_, Option<Vec<u8>>>(2)?.unwrap_or_default(),
                    value: row.get(3)?,
                })
            },
        )
        .storage()?
        .collect::<Result<Vec<_>, _>>()
        .storage()
}

fn restore_group(
    transaction: &rusqlite::Transaction<'_>,
    group_id: &GroupId,
    group: &Group,
) -> StorageResult<()> {
    transaction
        .execute(
            "INSERT INTO marmot_groups (id, epoch, record)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                epoch = excluded.epoch,
                record = excluded.record",
            params![
                group_id.as_slice(),
                epoch_to_i64(group.epoch)?,
                serialize(group)?
            ],
        )
        .storage()?;

    Ok(())
}

fn restore_messages(
    transaction: &rusqlite::Transaction<'_>,
    group_id: &GroupId,
    messages: &[OrderedMessage],
) -> StorageResult<()> {
    transaction
        .execute(
            "DELETE FROM marmot_messages WHERE group_id = ?1",
            params![group_id.as_slice()],
        )
        .storage()?;
    for message in messages {
        transaction
            .execute(
                "INSERT INTO marmot_messages
                    (insert_order, id, group_id, epoch, state, record)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    message.insert_order,
                    message.record.id.as_slice(),
                    message.record.group_id.as_slice(),
                    epoch_to_i64(message.record.epoch)?,
                    message_state_to_i64(message.record.state),
                    serialize(&message.record)?
                ],
            )
            .storage()?;
    }

    Ok(())
}

fn restore_queued_outbound(
    transaction: &rusqlite::Transaction<'_>,
    group_id: &GroupId,
    queued_outbound: &[OrderedQueuedOutbound],
) -> StorageResult<()> {
    transaction
        .execute(
            "DELETE FROM marmot_queued_outbound WHERE group_id = ?1",
            params![group_id.as_slice()],
        )
        .storage()?;
    for queued in queued_outbound {
        transaction
            .execute(
                "INSERT INTO marmot_queued_outbound
                    (insert_order, id, group_id, created_at_ms, record)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    queued.insert_order,
                    queued.record.id.as_slice(),
                    queued.record.group_id.as_slice(),
                    created_at_to_i64(queued.record.created_at_ms)?,
                    serialize(&queued.record)?
                ],
            )
            .storage()?;
    }

    Ok(())
}

fn restore_member_capabilities(
    transaction: &rusqlite::Transaction<'_>,
    group_id: &GroupId,
    member_caps: &[MemberCapabilitiesSnapshot],
) -> StorageResult<()> {
    transaction
        .execute(
            "DELETE FROM marmot_member_capabilities WHERE group_id = ?1",
            params![group_id.as_slice()],
        )
        .storage()?;
    for caps in member_caps {
        transaction
            .execute(
                "INSERT INTO marmot_member_capabilities (group_id, member_id, capabilities)
                 VALUES (?1, ?2, ?3)",
                params![
                    group_id.as_slice(),
                    caps.member_id.as_slice(),
                    serialize(&caps.capabilities)?
                ],
            )
            .storage()?;
    }

    Ok(())
}

fn restore_convergence_policy(
    transaction: &rusqlite::Transaction<'_>,
    group_id: &GroupId,
    policy: Option<&[u8]>,
) -> StorageResult<()> {
    transaction
        .execute(
            "DELETE FROM marmot_convergence_policies WHERE group_id = ?1",
            params![group_id.as_slice()],
        )
        .storage()?;
    if let Some(policy) = policy {
        transaction
            .execute(
                "INSERT INTO marmot_convergence_policies (group_id, policy)
                 VALUES (?1, ?2)",
                params![group_id.as_slice(), policy],
            )
            .storage()?;
    }

    Ok(())
}

fn restore_openmls_values(
    transaction: &rusqlite::Transaction<'_>,
    mls_group_key: &[u8],
    values: &[OpenMlsValueSnapshot],
) -> StorageResult<()> {
    transaction
        .execute(
            "DELETE FROM marmot_openmls_values
             WHERE provider_version = ?1 AND group_key = ?2",
            params![openmls_traits::storage::CURRENT_VERSION, mls_group_key],
        )
        .storage()?;
    for value in values {
        transaction
            .execute(
                "INSERT INTO marmot_openmls_values
                    (provider_version, label, storage_key, group_key, value)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    openmls_traits::storage::CURRENT_VERSION,
                    value.label,
                    value.storage_key,
                    value.group_key,
                    value.value
                ],
            )
            .storage()?;
    }

    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    group: Group,
    messages: Vec<OrderedMessage>,
    queued_outbound: Vec<OrderedQueuedOutbound>,
    member_caps: Vec<MemberCapabilitiesSnapshot>,
    convergence_policy: Option<Vec<u8>>,
    openmls_values: Vec<OpenMlsValueSnapshot>,
}

#[derive(Serialize, Deserialize)]
struct OrderedMessage {
    insert_order: i64,
    record: MessageRecord,
}

#[derive(Serialize, Deserialize)]
struct OrderedQueuedOutbound {
    insert_order: i64,
    record: QueuedOutboundIntent,
}

#[derive(Serialize, Deserialize)]
struct MemberCapabilitiesSnapshot {
    member_id: MemberId,
    capabilities: GroupCapabilities,
}

#[derive(Serialize, Deserialize)]
struct OpenMlsValueSnapshot {
    label: Vec<u8>,
    storage_key: Vec<u8>,
    group_key: Vec<u8>,
    value: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use cgka_traits::capabilities::{Capability, GroupCapabilities};
    use cgka_traits::group::Group;
    use cgka_traits::message::MessageState;
    use cgka_traits::storage::{
        CapabilityStorage, ConvergencePolicyStorage, GroupStorage, MessageStorage,
        OutboundIntentStorage, StorageError, StorageProvider,
    };
    use cgka_traits::types::EpochId;
    use openmls_traits::storage::{
        CURRENT_VERSION, Entity, StorageProvider as OpenMlsStorageProvider, traits,
    };
    use serde::{Deserialize, Serialize};

    use super::super::WhitenoiseMarmotStorage;
    use super::super::test_support::{
        gid, mid, sample_group, sample_message, sample_queued_intent,
    };

    #[test]
    fn message_state_transitions() {
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        let message = sample_message(mid(1), gid(1), 0);

        storage.put_message(&message).unwrap();
        assert_eq!(
            storage.get_message(&message.id).unwrap().state,
            MessageState::Created
        );

        storage
            .update_message_state(&message.id, MessageState::Retryable)
            .unwrap();
        assert_eq!(
            storage.get_message(&message.id).unwrap().state,
            MessageState::Retryable
        );

        storage
            .update_message_state(&message.id, MessageState::PeelDeferred)
            .unwrap();
        assert_eq!(
            storage.get_message(&message.id).unwrap().state,
            MessageState::PeelDeferred
        );
    }

    #[test]
    fn update_message_state_keeps_read_modify_write_in_one_transaction() {
        let source = include_str!("messages.rs");
        let body = source
            .split("fn update_message_state")
            .nth(1)
            .expect("update_message_state body");

        assert!(body.contains("transaction()"));
        assert!(!body.contains("self.get_message(id)"));
    }

    #[test]
    fn list_messages_filters_by_group_epoch_and_insert_order() {
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        storage
            .put_message(&sample_message(mid(3), gid(1), 0))
            .unwrap();
        storage
            .put_message(&sample_message(mid(1), gid(1), 5))
            .unwrap();
        storage
            .put_message(&sample_message(mid(2), gid(2), 9))
            .unwrap();

        let ids: Vec<_> = storage
            .list_messages(&gid(1), EpochId(0))
            .unwrap()
            .into_iter()
            .map(|message| message.id)
            .collect();
        assert_eq!(ids, vec![mid(3), mid(1)]);

        let ids: Vec<_> = storage
            .list_messages(&gid(1), EpochId(3))
            .unwrap()
            .into_iter()
            .map(|message| message.id)
            .collect();
        assert_eq!(ids, vec![mid(1)]);
    }

    #[test]
    fn snapshot_rollback_restores_group_messages_queue_caps_policy_and_openmls_state() {
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        let g0 = sample_group(1, 0, 1);
        storage.put_group(&g0).unwrap();
        storage
            .put_message(&sample_message(mid(1), g0.id.clone(), 0))
            .unwrap();
        storage
            .put_queued_outbound_intent(&sample_queued_intent(mid(10), g0.id.clone()))
            .unwrap();
        storage
            .put_convergence_policy(&g0.id, b"policy-v0")
            .unwrap();
        let mut caps = GroupCapabilities::default();
        caps.insert(Capability::Proposal(10));
        storage
            .save_member_capabilities(&g0.id, &g0.members[0], caps.clone())
            .unwrap();
        let mls_group_id = openmls::group::GroupId::from_slice(g0.id.as_slice());
        storage
            .mls_storage()
            .write_group_state(&mls_group_id, &TestGroupState(b"epoch-0".to_vec()))
            .unwrap();

        storage.create_group_snapshot(&g0.id, "pre-commit").unwrap();

        let g1 = Group {
            epoch: EpochId(1),
            name: "changed".to_string(),
            ..g0.clone()
        };
        storage.put_group(&g1).unwrap();
        storage
            .put_message(&sample_message(mid(2), g0.id.clone(), 1))
            .unwrap();
        storage.delete_queued_outbound_intent(&mid(10)).unwrap();
        storage
            .put_convergence_policy(&g0.id, b"policy-v1")
            .unwrap();
        storage
            .mls_storage()
            .write_group_state(&mls_group_id, &TestGroupState(b"epoch-1".to_vec()))
            .unwrap();

        storage
            .rollback_group_to_snapshot(&g0.id, "pre-commit")
            .unwrap();

        assert_eq!(storage.get_group(&g0.id).unwrap(), g0);
        let messages = storage.list_messages(&g0.id, EpochId(0)).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, mid(1));
        let queued = storage.list_queued_outbound_intents(&g0.id).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].id, mid(10));
        assert_eq!(
            storage.convergence_policy(&g0.id).unwrap(),
            Some(b"policy-v0".to_vec())
        );
        assert_eq!(
            storage
                .member_capabilities(&g0.id, &g0.members[0].id)
                .unwrap(),
            Some(caps)
        );
        let state: Option<TestGroupState> =
            storage.mls_storage().group_state(&mls_group_id).unwrap();
        assert_eq!(state, Some(TestGroupState(b"epoch-0".to_vec())));
    }

    #[test]
    fn snapshot_listing_and_release_are_group_scoped() {
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        let g1 = sample_group(1, 0, 0);
        let g2 = sample_group(2, 0, 0);
        storage.put_group(&g1).unwrap();
        storage.put_group(&g2).unwrap();

        storage.create_group_snapshot(&g1.id, "z-after").unwrap();
        storage
            .create_group_snapshot(&g2.id, "other-group")
            .unwrap();
        storage.create_group_snapshot(&g1.id, "a-before").unwrap();

        assert_eq!(
            storage.list_group_snapshots(&g1.id).unwrap(),
            vec!["a-before".to_string(), "z-after".to_string()]
        );

        storage.release_group_snapshot(&g1.id, "a-before").unwrap();

        assert_eq!(
            storage.list_group_snapshots(&g1.id).unwrap(),
            vec!["z-after".to_string()]
        );
        assert!(matches!(
            storage.rollback_group_to_snapshot(&g1.id, "a-before"),
            Err(StorageError::SnapshotMissing(_))
        ));
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct TestGroupState(Vec<u8>);

    impl Entity<CURRENT_VERSION> for TestGroupState {}
    impl traits::GroupState<CURRENT_VERSION> for TestGroupState {}
}

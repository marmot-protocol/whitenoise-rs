mod account_device_signer;
mod capabilities;
mod codec;
mod connection;
mod convergence_policy;
mod groups;
mod key_packages;
mod keyring;
mod messages;
mod migrations;
mod openmls_storage;
mod outbound;
mod result_ext;
mod storage_provider;
mod welcomes;

use std::sync::{Arc, Mutex};

use openmls_storage::WhitenoiseOpenMlsStorage;

type SharedConnection = Arc<Mutex<rusqlite::Connection>>;

#[derive(Clone)]
pub(crate) struct WhitenoiseMarmotStorage {
    connection: SharedConnection,
    openmls: WhitenoiseOpenMlsStorage,
}

#[cfg(test)]
mod tests {
    use cgka_traits::capabilities::GroupCapabilities;
    use cgka_traits::group::{Group, Member};
    use cgka_traits::storage::{
        AccountDeviceSignerBinding, AccountDeviceSignerStorage, ConvergencePolicyStorage,
        GroupStorage, StorageError,
    };
    use cgka_traits::types::{EpochId, GroupId, MemberId};
    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::WhitenoiseMarmotStorage;
    use super::connection::SqlCipherKey;
    use super::migrations::run_migrations;

    #[test]
    fn convergence_policy_is_group_scoped_and_updateable() {
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        let first_group = GroupId::new(vec![1; 32]);
        let second_group = GroupId::new(vec![2; 32]);

        assert_eq!(storage.convergence_policy(&first_group).unwrap(), None);

        storage
            .put_convergence_policy(&first_group, b"policy-v1")
            .unwrap();
        storage
            .put_convergence_policy(&second_group, b"other-policy")
            .unwrap();
        storage
            .put_convergence_policy(&first_group, b"policy-v2")
            .unwrap();

        assert_eq!(
            storage.convergence_policy(&first_group).unwrap(),
            Some(b"policy-v2".to_vec())
        );
        assert_eq!(
            storage.convergence_policy(&second_group).unwrap(),
            Some(b"other-policy".to_vec())
        );
    }

    #[test]
    fn encrypted_storage_reopens_with_existing_policy() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("session.sqlite");
        let key = SqlCipherKey::from_slice(&[7; 32]).unwrap();
        let group_id = GroupId::new(vec![3; 32]);

        {
            let storage = WhitenoiseMarmotStorage::open_encrypted(&path, &key).unwrap();
            storage
                .put_convergence_policy(&group_id, b"persistent-policy")
                .unwrap();
        }

        let reopened = WhitenoiseMarmotStorage::open_encrypted(&path, &key).unwrap();

        assert_eq!(
            reopened.convergence_policy(&group_id).unwrap(),
            Some(b"persistent-policy".to_vec())
        );
    }

    #[test]
    fn account_device_signer_binding_is_identity_scoped_and_persistent() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("session.sqlite");
        let key = SqlCipherKey::from_slice(&[9; 32]).unwrap();
        let marmot_identity = MemberId::new(vec![4; 32]);
        let other_identity = MemberId::new(vec![5; 32]);
        let binding = AccountDeviceSignerBinding {
            marmot_identity: marmot_identity.clone(),
            mls_signature_public_key: vec![1, 2, 3, 4],
        };

        {
            let storage = WhitenoiseMarmotStorage::open_encrypted(&path, &key).unwrap();
            assert_eq!(
                storage.account_device_signer(&marmot_identity).unwrap(),
                None
            );

            storage.put_account_device_signer(&binding).unwrap();

            assert_eq!(
                storage.account_device_signer(&other_identity).unwrap(),
                None
            );
        }

        let reopened = WhitenoiseMarmotStorage::open_encrypted(&path, &key).unwrap();

        assert_eq!(
            reopened.account_device_signer(&marmot_identity).unwrap(),
            Some(binding)
        );
    }

    #[test]
    fn group_roundtrip_preserves_every_field() {
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        let group = sample_group(1, 7, 2);

        storage.put_group(&group).unwrap();

        assert_eq!(storage.get_group(&group.id).unwrap(), group);
    }

    #[test]
    fn group_update_preserves_group_scoped_rows() {
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        let group = sample_group(1, 0, 1);
        storage.put_group(&group).unwrap();
        storage
            .put_convergence_policy(&group.id, b"policy")
            .unwrap();

        let updated = Group {
            name: "updated".to_string(),
            epoch: EpochId(3),
            ..group.clone()
        };
        storage.put_group(&updated).unwrap();

        assert_eq!(storage.get_group(&group.id).unwrap(), updated);
        assert_eq!(
            storage.convergence_policy(&group.id).unwrap(),
            Some(b"policy".to_vec())
        );
    }

    #[test]
    fn group_delete_removes_group_scoped_rows() {
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        let group = sample_group(1, 0, 1);
        storage.put_group(&group).unwrap();
        storage
            .put_convergence_policy(&group.id, b"policy")
            .unwrap();

        storage.delete_group(&group.id).unwrap();

        assert!(matches!(
            storage.get_group(&group.id),
            Err(StorageError::NotFound)
        ));
        assert_eq!(storage.convergence_policy(&group.id).unwrap(), None);
    }

    #[test]
    fn group_missing_returns_not_found() {
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();

        assert!(matches!(
            storage.get_group(&GroupId::new(vec![9; 32])),
            Err(StorageError::NotFound)
        ));
        assert!(matches!(
            storage.delete_group(&GroupId::new(vec![9; 32])),
            Err(StorageError::NotFound)
        ));
    }

    #[test]
    fn list_groups_returns_ids_in_deterministic_order() {
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        let first = sample_group(1, 0, 0);
        let second = sample_group(2, 0, 0);

        storage.put_group(&second).unwrap();
        storage.put_group(&first).unwrap();

        assert_eq!(storage.list_groups().unwrap(), vec![first.id, second.id]);
    }

    #[test]
    fn migration_runner_rejects_unknown_future_versions() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                r#"
CREATE TABLE marmot_schema_migrations (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);
INSERT INTO marmot_schema_migrations (version, name)
VALUES (99, 'future_migration');
"#,
            )
            .unwrap();

        let err = run_migrations(&mut connection).unwrap_err();

        assert!(
            err.to_string()
                .contains("storage was migrated by a newer WhiteNoise version"),
            "{err}"
        );
    }

    #[test]
    fn migration_runner_rejects_version_name_mismatch() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                r#"
CREATE TABLE marmot_schema_migrations (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);
INSERT INTO marmot_schema_migrations (version, name)
VALUES (1, 'wrong_name');
"#,
            )
            .unwrap();

        let err = run_migrations(&mut connection).unwrap_err();

        assert!(
            err.to_string().contains("migration 1 changed name"),
            "{err}"
        );
    }

    fn sample_group(id_byte: u8, epoch: u64, members: usize) -> Group {
        Group {
            id: GroupId::new(vec![id_byte; 32]),
            name: format!("group-{id_byte}"),
            description: format!("description-{id_byte}"),
            epoch: EpochId(epoch),
            members: (0..members)
                .map(|index| Member {
                    id: MemberId::new(vec![id_byte + index as u8; 32]),
                    credential: vec![index as u8, id_byte],
                })
                .collect(),
            required_capabilities: GroupCapabilities::default(),
        }
    }
}

#[cfg(test)]
mod test_support {
    use cgka_traits::capabilities::GroupCapabilities;
    use cgka_traits::engine::SendIntent;
    use cgka_traits::group::{Group, Member};
    use cgka_traits::message::{MessageRecord, MessageState};
    use cgka_traits::storage::QueuedOutboundIntent;
    use cgka_traits::types::{EpochId, GroupId, MemberId, MessageId};
    use cgka_traits::welcome::PendingWelcome;

    pub(super) fn gid(byte: u8) -> GroupId {
        GroupId::new(vec![byte; 32])
    }

    pub(super) fn mid(byte: u8) -> MessageId {
        MessageId::new(vec![byte; 32])
    }

    pub(super) fn member_id(byte: u8) -> MemberId {
        MemberId::new(vec![byte; 32])
    }

    pub(super) fn sample_queued_intent(
        message_id: MessageId,
        group_id: GroupId,
    ) -> QueuedOutboundIntent {
        QueuedOutboundIntent {
            id: message_id,
            group_id: group_id.clone(),
            intent: SendIntent::AppMessage {
                group_id,
                payload: b"hello".to_vec(),
            },
            created_at_ms: 42,
        }
    }

    pub(super) fn sample_welcome(message_id: MessageId, group_id: GroupId) -> PendingWelcome {
        PendingWelcome {
            message_id,
            group_id,
            welcome_bytes: vec![1, 2, 3],
        }
    }

    pub(super) fn sample_message(
        message_id: MessageId,
        group_id: GroupId,
        epoch: u64,
    ) -> MessageRecord {
        MessageRecord {
            id: message_id,
            group_id,
            epoch: EpochId(epoch),
            state: MessageState::Created,
            payload: b"message".to_vec(),
        }
    }

    pub(super) fn sample_group(id_byte: u8, epoch: u64, members: usize) -> Group {
        Group {
            id: gid(id_byte),
            name: format!("group-{id_byte}"),
            description: format!("description-{id_byte}"),
            epoch: EpochId(epoch),
            members: (0..members)
                .map(|index| Member {
                    id: member_id(id_byte + index as u8),
                    credential: vec![index as u8, id_byte],
                })
                .collect(),
            required_capabilities: GroupCapabilities::default(),
        }
    }
}

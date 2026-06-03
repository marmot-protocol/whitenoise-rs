use cgka_traits::storage::{StorageError, StorageResult, WelcomeStorage};
use cgka_traits::types::MessageId;
use cgka_traits::welcome::PendingWelcome;
use rusqlite::{OptionalExtension, params};

use super::WhitenoiseMarmotStorage;
use super::codec::{deserialize, serialize};
use super::result_ext::RusqliteResultExt;

impl WelcomeStorage for WhitenoiseMarmotStorage {
    fn put_welcome(&self, welcome: &PendingWelcome) -> StorageResult<()> {
        self.lock()
            .execute(
                "INSERT OR REPLACE INTO marmot_welcomes (message_id, group_id, record)
                 VALUES (?1, ?2, ?3)",
                params![
                    welcome.message_id.as_slice(),
                    welcome.group_id.as_slice(),
                    serialize(welcome)?
                ],
            )
            .storage()?;

        Ok(())
    }

    fn take_welcome(&self, id: &MessageId) -> StorageResult<PendingWelcome> {
        let mut connection = self.lock();
        let transaction = connection.transaction().storage()?;
        let record: Vec<u8> = transaction
            .query_row(
                "SELECT record FROM marmot_welcomes WHERE message_id = ?1",
                params![id.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .storage()?
            .ok_or(StorageError::NotFound)?;
        let welcome = deserialize(&record)?;
        transaction
            .execute(
                "DELETE FROM marmot_welcomes WHERE message_id = ?1",
                params![id.as_slice()],
            )
            .storage()?;
        transaction.commit().storage()?;

        Ok(welcome)
    }

    fn list_welcomes(&self) -> StorageResult<Vec<PendingWelcome>> {
        let connection = self.lock();
        let mut statement = connection
            .prepare("SELECT record FROM marmot_welcomes ORDER BY rowid")
            .storage()?;
        let records = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .storage()?
            .collect::<Result<Vec<_>, _>>()
            .storage()?;

        records.iter().map(|record| deserialize(record)).collect()
    }
}

#[cfg(test)]
mod tests {
    use cgka_traits::storage::{StorageError, WelcomeStorage};

    use super::super::WhitenoiseMarmotStorage;
    use super::super::test_support::{gid, mid, sample_welcome};

    #[test]
    fn welcome_take_is_one_shot() {
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        let welcome = sample_welcome(mid(1), gid(1));

        storage.put_welcome(&welcome).unwrap();

        assert_eq!(storage.list_welcomes().unwrap(), vec![welcome.clone()]);
        assert_eq!(storage.take_welcome(&mid(1)).unwrap(), welcome);
        assert!(matches!(
            storage.take_welcome(&mid(1)),
            Err(StorageError::NotFound)
        ));
    }

    #[test]
    fn take_welcome_retains_row_when_decode_fails() {
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        storage
            .lock()
            .execute(
                "INSERT INTO marmot_welcomes (message_id, group_id, record)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![mid(1).as_slice(), gid(1).as_slice(), b"not json"],
            )
            .unwrap();

        assert!(matches!(
            storage.take_welcome(&mid(1)),
            Err(StorageError::Serialization(_))
        ));
        let remaining: i64 = storage
            .lock()
            .query_row("SELECT count(*) FROM marmot_welcomes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }
}

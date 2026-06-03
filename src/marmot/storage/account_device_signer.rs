use cgka_traits::storage::{AccountDeviceSignerBinding, AccountDeviceSignerStorage, StorageResult};
use cgka_traits::types::MemberId;
use rusqlite::{OptionalExtension, params};

use super::WhitenoiseMarmotStorage;
use super::codec::{deserialize, serialize};
use super::result_ext::RusqliteResultExt;

impl AccountDeviceSignerStorage for WhitenoiseMarmotStorage {
    fn put_account_device_signer(&self, binding: &AccountDeviceSignerBinding) -> StorageResult<()> {
        self.lock()
            .execute(
                "INSERT OR REPLACE INTO marmot_account_device_signers (marmot_identity, record)
                 VALUES (?1, ?2)",
                params![binding.marmot_identity.as_slice(), serialize(binding)?],
            )
            .storage()?;

        Ok(())
    }

    fn account_device_signer(
        &self,
        marmot_identity: &MemberId,
    ) -> StorageResult<Option<AccountDeviceSignerBinding>> {
        let record: Option<Vec<u8>> = self
            .lock()
            .query_row(
                "SELECT record FROM marmot_account_device_signers WHERE marmot_identity = ?1",
                params![marmot_identity.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .storage()?;

        record.as_deref().map(deserialize).transpose()
    }
}

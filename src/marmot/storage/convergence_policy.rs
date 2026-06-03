use cgka_traits::storage::{ConvergencePolicyStorage, StorageResult};
use cgka_traits::types::GroupId;
use rusqlite::{OptionalExtension, params};

use super::WhitenoiseMarmotStorage;
use super::result_ext::RusqliteResultExt;

impl ConvergencePolicyStorage for WhitenoiseMarmotStorage {
    fn put_convergence_policy(&self, group_id: &GroupId, policy: &[u8]) -> StorageResult<()> {
        self.lock()
            .execute(
                "INSERT OR REPLACE INTO marmot_convergence_policies (group_id, policy)
                 VALUES (?1, ?2)",
                params![group_id.as_slice(), policy],
            )
            .storage()?;

        Ok(())
    }

    fn convergence_policy(&self, group_id: &GroupId) -> StorageResult<Option<Vec<u8>>> {
        self.lock()
            .query_row(
                "SELECT policy FROM marmot_convergence_policies WHERE group_id = ?1",
                params![group_id.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .storage()
    }
}

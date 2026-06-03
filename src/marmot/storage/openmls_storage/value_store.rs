use serde::de::DeserializeOwned;

use super::labels::build_key;
use super::{OpenMlsStorageError, WhitenoiseOpenMlsStorage};
use openmls_traits::storage::{CURRENT_VERSION, Entity, Key};
use rusqlite::{OptionalExtension, params};

impl WhitenoiseOpenMlsStorage {
    pub(in crate::marmot::storage::openmls_storage) fn write_value(
        &self,
        label: &[u8],
        key: Vec<u8>,
        group_key: Option<Vec<u8>>,
        value: Vec<u8>,
    ) -> Result<(), OpenMlsStorageError> {
        let storage_key = build_key(label, key);
        self.lock().execute(
            "INSERT OR REPLACE INTO marmot_openmls_values
                (provider_version, label, storage_key, group_key, value)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                CURRENT_VERSION,
                label,
                storage_key,
                group_key.as_deref(),
                value
            ],
        )?;
        Ok(())
    }

    pub(in crate::marmot::storage::openmls_storage) fn write_entity<T: Entity<CURRENT_VERSION>>(
        &self,
        label: &[u8],
        key: Vec<u8>,
        group_key: Option<Vec<u8>>,
        value: &T,
    ) -> Result<(), OpenMlsStorageError> {
        self.write_value(label, key, group_key, serde_json::to_vec(value)?)
    }

    pub(in crate::marmot::storage::openmls_storage) fn write_group_entity<
        GroupId: Key<CURRENT_VERSION>,
        T: Entity<CURRENT_VERSION>,
    >(
        &self,
        label: &[u8],
        group_id: &GroupId,
        value: &T,
    ) -> Result<(), OpenMlsStorageError> {
        let group_key = Self::group_key(group_id)?;
        self.write_entity(label, group_key.clone(), Some(group_key), value)
    }

    pub(in crate::marmot::storage::openmls_storage) fn append_entity<T: Entity<CURRENT_VERSION>>(
        &self,
        label: &[u8],
        key: Vec<u8>,
        group_key: Option<Vec<u8>>,
        value: &T,
    ) -> Result<(), OpenMlsStorageError> {
        let storage_key = build_key(label, key);
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let mut list: Vec<Vec<u8>> = transaction
            .query_row(
                "SELECT value FROM marmot_openmls_values
                 WHERE provider_version = ?1 AND storage_key = ?2",
                params![CURRENT_VERSION, storage_key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .map(|value| serde_json::from_slice(&value))
            .transpose()?
            .unwrap_or_default();
        list.push(serde_json::to_vec(value)?);
        transaction.execute(
            "INSERT OR REPLACE INTO marmot_openmls_values
                (provider_version, label, storage_key, group_key, value)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                CURRENT_VERSION,
                label,
                storage_key,
                group_key.as_deref(),
                serde_json::to_vec(&list)?
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(in crate::marmot::storage::openmls_storage) fn remove_entity<T: Entity<CURRENT_VERSION>>(
        &self,
        label: &[u8],
        key: Vec<u8>,
        group_key: Option<Vec<u8>>,
        value: &T,
    ) -> Result<(), OpenMlsStorageError> {
        let encoded = serde_json::to_vec(value)?;
        let storage_key = build_key(label, key);
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let mut list: Vec<Vec<u8>> = transaction
            .query_row(
                "SELECT value FROM marmot_openmls_values
                 WHERE provider_version = ?1 AND storage_key = ?2",
                params![CURRENT_VERSION, storage_key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .map(|value| serde_json::from_slice(&value))
            .transpose()?
            .unwrap_or_default();
        if let Some(pos) = list.iter().position(|stored| stored == &encoded) {
            list.remove(pos);
        }
        transaction.execute(
            "INSERT OR REPLACE INTO marmot_openmls_values
                (provider_version, label, storage_key, group_key, value)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                CURRENT_VERSION,
                label,
                storage_key,
                group_key.as_deref(),
                serde_json::to_vec(&list)?
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn read_raw_list(
        &self,
        label: &[u8],
        key: &[u8],
    ) -> Result<Option<Vec<Vec<u8>>>, OpenMlsStorageError> {
        let storage_key = build_key(label, key.to_vec());
        let value: Option<Vec<u8>> = self
            .lock()
            .query_row(
                "SELECT value FROM marmot_openmls_values
                 WHERE provider_version = ?1 AND storage_key = ?2",
                params![CURRENT_VERSION, storage_key],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|value| serde_json::from_slice(&value).map_err(Into::into))
            .transpose()
    }

    pub(in crate::marmot::storage::openmls_storage) fn read_entity<T: Entity<CURRENT_VERSION>>(
        &self,
        label: &[u8],
        key: Vec<u8>,
    ) -> Result<Option<T>, OpenMlsStorageError> {
        self.read_json(label, key)
    }

    pub(in crate::marmot::storage::openmls_storage) fn read_json<T: DeserializeOwned>(
        &self,
        label: &[u8],
        key: Vec<u8>,
    ) -> Result<Option<T>, OpenMlsStorageError> {
        let storage_key = build_key(label, key);
        let value: Option<Vec<u8>> = self
            .lock()
            .query_row(
                "SELECT value FROM marmot_openmls_values
                 WHERE provider_version = ?1 AND storage_key = ?2",
                params![CURRENT_VERSION, storage_key],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|value| serde_json::from_slice(&value).map_err(Into::into))
            .transpose()
    }

    pub(in crate::marmot::storage::openmls_storage) fn read_group_entity<
        GroupId: Key<CURRENT_VERSION>,
        T: Entity<CURRENT_VERSION>,
    >(
        &self,
        label: &[u8],
        group_id: &GroupId,
    ) -> Result<Option<T>, OpenMlsStorageError> {
        self.read_entity(label, Self::group_key(group_id)?)
    }

    pub(in crate::marmot::storage::openmls_storage) fn read_list<T: Entity<CURRENT_VERSION>>(
        &self,
        label: &[u8],
        key: Vec<u8>,
    ) -> Result<Vec<T>, OpenMlsStorageError> {
        self.read_raw_list(label, &key)?
            .unwrap_or_default()
            .into_iter()
            .map(|value| serde_json::from_slice(&value).map_err(Into::into))
            .collect()
    }

    pub(in crate::marmot::storage::openmls_storage) fn delete_value(
        &self,
        label: &[u8],
        key: Vec<u8>,
    ) -> Result<(), OpenMlsStorageError> {
        let storage_key = build_key(label, key);
        self.lock().execute(
            "DELETE FROM marmot_openmls_values
             WHERE provider_version = ?1 AND storage_key = ?2",
            params![CURRENT_VERSION, storage_key],
        )?;
        Ok(())
    }

    pub(in crate::marmot::storage::openmls_storage) fn delete_group_value<
        GroupId: Key<CURRENT_VERSION>,
    >(
        &self,
        label: &[u8],
        group_id: &GroupId,
    ) -> Result<(), OpenMlsStorageError> {
        self.delete_value(label, Self::group_key(group_id)?)
    }

    pub(in crate::marmot::storage::openmls_storage) fn delete_group_labels<
        GroupId: Key<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        labels: &[&[u8]],
    ) -> Result<(), OpenMlsStorageError> {
        let group_key = Self::group_key(group_id)?;
        let connection = self.lock();
        for label in labels {
            connection.execute(
                "DELETE FROM marmot_openmls_values
                 WHERE provider_version = ?1 AND group_key = ?2 AND label = ?3",
                params![CURRENT_VERSION, group_key, *label],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn list_mutations_keep_read_modify_write_under_one_transaction() {
        let source = include_str!("value_store.rs");
        for function in ["append_entity", "remove_entity"] {
            let body = source
                .split(&format!("fn {function}"))
                .nth(1)
                .expect("function body");
            let body = body
                .split("pub(in crate::marmot::storage::openmls_storage) fn")
                .next()
                .unwrap_or(body);
            let body = body.split("\n    fn ").next().unwrap_or(body);

            assert!(body.contains("transaction()"), "{function}");
            assert!(!body.contains("read_raw_list"), "{function}");
            assert!(!body.contains("self.write_value"), "{function}");
        }
    }
}

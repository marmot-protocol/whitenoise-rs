use cgka_traits::engine::KeyPackage;
use cgka_traits::storage::{StorageError, StorageProvider, StorageResult};
use openmls::ciphersuite::hash_ref::KeyPackageRef;
#[cfg(test)]
use openmls::prelude::KeyPackageBundle;
use openmls::prelude::{MlsMessageBodyIn, MlsMessageIn, ProtocolVersion};
use openmls_rust_crypto::RustCrypto;
use openmls_traits::storage::StorageProvider as OpenMlsStorageProvider;
use tls_codec::Deserialize as _;

use super::WhitenoiseMarmotStorage;

impl WhitenoiseMarmotStorage {
    pub(crate) fn delete_key_package_material(
        &self,
        key_package: &KeyPackage,
    ) -> StorageResult<()> {
        let key_package_ref = key_package_ref(key_package)?;
        self.mls_storage()
            .delete_key_package(&key_package_ref)
            .map_err(openmls_storage_error)
    }

    #[cfg(test)]
    pub(crate) fn has_key_package_material(&self, key_package: &KeyPackage) -> StorageResult<bool> {
        let key_package_ref = key_package_ref(key_package)?;
        let stored: Option<KeyPackageBundle> = self
            .mls_storage()
            .key_package(&key_package_ref)
            .map_err(openmls_storage_error)?;
        Ok(stored.is_some())
    }
}

fn key_package_ref(key_package: &KeyPackage) -> StorageResult<KeyPackageRef> {
    let msg = MlsMessageIn::tls_deserialize_exact(key_package.bytes()).map_err(|err| {
        StorageError::Serialization(format!("key package deserialize failed: {err:?}"))
    })?;
    let key_package_in = match msg.extract() {
        MlsMessageBodyIn::KeyPackage(key_package) => key_package,
        _ => {
            return Err(StorageError::Serialization(
                "MLS message did not carry a KeyPackage".to_string(),
            ));
        }
    };

    let crypto = RustCrypto::default();
    let key_package = key_package_in
        .validate(&crypto, ProtocolVersion::Mls10)
        .map_err(|err| StorageError::Backend(format!("key package validate failed: {err:?}")))?;
    key_package
        .hash_ref(&crypto)
        .map_err(|err| StorageError::Backend(format!("key package ref failed: {err:?}")))
}

fn openmls_storage_error<E>(err: E) -> StorageError
where
    E: std::fmt::Display,
{
    StorageError::Backend(err.to_string())
}

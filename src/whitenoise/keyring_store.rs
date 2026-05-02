use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use keyring_core::api::{CredentialApi, CredentialStoreApi};
use keyring_core::{Credential, CredentialPersistence, CredentialStore, Entry, Error, Result};

pub(crate) const LINUX_SECRET_SERVICE_TARGET: &str = "whitenoise";

pub(crate) struct TargetedCredentialStore {
    inner: Arc<CredentialStore>,
    target: &'static str,
}

impl TargetedCredentialStore {
    pub(crate) fn new(inner: Arc<CredentialStore>, target: &'static str) -> Arc<Self> {
        Arc::new(Self { inner, target })
    }

    fn with_target<'a>(
        &'a self,
        modifiers: Option<&HashMap<&'a str, &'a str>>,
    ) -> HashMap<&'a str, &'a str> {
        let mut targeted = modifiers.cloned().unwrap_or_default();
        targeted.entry("target").or_insert(self.target);
        targeted
    }
}

impl fmt::Debug for TargetedCredentialStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetedCredentialStore")
            .field("inner", &self.inner)
            .field("target", &self.target)
            .finish()
    }
}

impl CredentialStoreApi for TargetedCredentialStore {
    fn vendor(&self) -> String {
        self.inner.vendor()
    }

    fn id(&self) -> String {
        self.inner.id()
    }

    fn build(
        &self,
        service: &str,
        user: &str,
        modifiers: Option<&HashMap<&str, &str>>,
    ) -> Result<Entry> {
        let targeted = self.with_target(modifiers);
        self.inner.build(service, user, Some(&targeted))
    }

    fn search(&self, spec: &HashMap<&str, &str>) -> Result<Vec<Entry>> {
        let targeted = self.with_target(Some(spec));
        self.inner.search(&targeted)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn persistence(&self) -> CredentialPersistence {
        self.inner.persistence()
    }

    fn debug_fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

pub(crate) struct LegacyMigrationCredentialStore {
    primary: Arc<CredentialStore>,
    legacy: Arc<CredentialStore>,
}

impl LegacyMigrationCredentialStore {
    pub(crate) fn new(primary: Arc<CredentialStore>, legacy: Arc<CredentialStore>) -> Arc<Self> {
        Arc::new(Self { primary, legacy })
    }
}

impl fmt::Debug for LegacyMigrationCredentialStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LegacyMigrationCredentialStore")
            .field("primary", &self.primary)
            .field("legacy", &self.legacy)
            .finish()
    }
}

impl CredentialStoreApi for LegacyMigrationCredentialStore {
    fn vendor(&self) -> String {
        format!(
            "{} with legacy migration from {}",
            self.primary.vendor(),
            self.legacy.vendor()
        )
    }

    fn id(&self) -> String {
        format!(
            "{} with legacy migration {}",
            self.primary.id(),
            self.legacy.id()
        )
    }

    fn build(
        &self,
        service: &str,
        user: &str,
        modifiers: Option<&HashMap<&str, &str>>,
    ) -> Result<Entry> {
        let primary = self.primary.build(service, user, modifiers)?;
        let legacy = self.legacy.build(service, user, modifiers)?;
        Ok(Entry::new_with_credential(Arc::new(
            LegacyMigrationCredential {
                service: service.to_string(),
                user: user.to_string(),
                primary,
                legacy,
            },
        )))
    }

    fn search(&self, spec: &HashMap<&str, &str>) -> Result<Vec<Entry>> {
        self.primary.search(spec)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn persistence(&self) -> CredentialPersistence {
        self.primary.persistence()
    }

    fn debug_fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

#[derive(Debug)]
struct LegacyMigrationCredential {
    service: String,
    user: String,
    primary: Entry,
    legacy: Entry,
}

impl LegacyMigrationCredential {
    fn delete_entry(entry: &Entry) -> Result<bool> {
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(Error::NoEntry) => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn migrate_legacy_secret(&self, secret: Vec<u8>) -> Result<Vec<u8>> {
        self.primary.set_secret(&secret)?;
        Self::delete_entry(&self.legacy)?;
        Ok(secret)
    }
}

impl CredentialApi for LegacyMigrationCredential {
    fn set_secret(&self, secret: &[u8]) -> Result<()> {
        self.primary.set_secret(secret)?;
        Self::delete_entry(&self.legacy)?;
        Ok(())
    }

    fn get_secret(&self) -> Result<Vec<u8>> {
        match self.legacy.get_secret() {
            Ok(secret) => self.migrate_legacy_secret(secret),
            Err(Error::NoEntry) => self.primary.get_secret(),
            Err(err) => Err(err),
        }
    }

    fn delete_credential(&self) -> Result<()> {
        let mut deleted = false;
        let mut first_error = None;

        for result in [
            Self::delete_entry(&self.primary),
            Self::delete_entry(&self.legacy),
        ] {
            match result {
                Ok(was_deleted) => deleted |= was_deleted,
                Err(err) => {
                    first_error.get_or_insert(err);
                }
            };
        }

        if let Some(err) = first_error {
            return Err(err);
        }

        if deleted { Ok(()) } else { Err(Error::NoEntry) }
    }

    fn get_credential(&self) -> Result<Option<Arc<Credential>>> {
        Ok(None)
    }

    fn get_specifiers(&self) -> Option<(String, String)> {
        Some((self.service.clone(), self.user.clone()))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn debug_fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use keyring_core::api::CredentialApi;
    use keyring_core::{Credential, Error};

    use super::*;

    #[derive(Default)]
    struct RecordingStore {
        build_modifiers: Mutex<Vec<HashMap<String, String>>>,
        credentials: Mutex<HashMap<(String, String), Arc<RecordingCredential>>>,
        search_specs: Mutex<Vec<HashMap<String, String>>>,
    }

    impl RecordingStore {
        fn last_build_modifier(&self, key: &str) -> Option<String> {
            self.build_modifiers
                .lock()
                .unwrap()
                .last()?
                .get(key)
                .cloned()
        }

        fn last_search_spec(&self, key: &str) -> Option<String> {
            self.search_specs.lock().unwrap().last()?.get(key).cloned()
        }
    }

    impl fmt::Debug for RecordingStore {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("RecordingStore").finish()
        }
    }

    impl CredentialStoreApi for RecordingStore {
        fn vendor(&self) -> String {
            "test-store".to_string()
        }

        fn id(&self) -> String {
            "test-store".to_string()
        }

        fn build(
            &self,
            service: &str,
            user: &str,
            modifiers: Option<&HashMap<&str, &str>>,
        ) -> Result<Entry> {
            self.build_modifiers
                .lock()
                .unwrap()
                .push(owned_map(modifiers));
            let mut credentials = self.credentials.lock().unwrap();
            let credential = credentials
                .entry((service.to_string(), user.to_string()))
                .or_insert_with(|| {
                    Arc::new(RecordingCredential {
                        service: service.to_string(),
                        user: user.to_string(),
                        secret: Mutex::new(None),
                    })
                })
                .clone();
            Ok(Entry::new_with_credential(credential))
        }

        fn search(&self, spec: &HashMap<&str, &str>) -> Result<Vec<Entry>> {
            self.search_specs
                .lock()
                .unwrap()
                .push(owned_map(Some(spec)));
            Ok(Vec::new())
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Debug)]
    struct RecordingCredential {
        service: String,
        user: String,
        secret: Mutex<Option<Vec<u8>>>,
    }

    impl CredentialApi for RecordingCredential {
        fn set_secret(&self, secret: &[u8]) -> Result<()> {
            *self.secret.lock().unwrap() = Some(secret.to_vec());
            Ok(())
        }

        fn get_secret(&self) -> Result<Vec<u8>> {
            self.secret.lock().unwrap().clone().ok_or(Error::NoEntry)
        }

        fn delete_credential(&self) -> Result<()> {
            match self.secret.lock().unwrap().take() {
                Some(_) => Ok(()),
                None => Err(Error::NoEntry),
            }
        }

        fn get_credential(&self) -> Result<Option<Arc<Credential>>> {
            Ok(None)
        }

        fn get_specifiers(&self) -> Option<(String, String)> {
            Some((self.service.clone(), self.user.clone()))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Debug)]
    struct TestCredential;

    impl CredentialApi for TestCredential {
        fn set_secret(&self, _secret: &[u8]) -> Result<()> {
            Ok(())
        }

        fn get_secret(&self) -> Result<Vec<u8>> {
            Err(Error::NoEntry)
        }

        fn delete_credential(&self) -> Result<()> {
            Ok(())
        }

        fn get_credential(&self) -> Result<Option<Arc<Credential>>> {
            Ok(None)
        }

        fn get_specifiers(&self) -> Option<(String, String)> {
            None
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct DeleteFailureStore {
        secret: Vec<u8>,
    }

    impl fmt::Debug for DeleteFailureStore {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("DeleteFailureStore").finish()
        }
    }

    impl CredentialStoreApi for DeleteFailureStore {
        fn vendor(&self) -> String {
            "delete-failure-store".to_string()
        }

        fn id(&self) -> String {
            "delete-failure-store".to_string()
        }

        fn build(
            &self,
            service: &str,
            user: &str,
            _modifiers: Option<&HashMap<&str, &str>>,
        ) -> Result<Entry> {
            Ok(Entry::new_with_credential(Arc::new(
                DeleteFailureCredential {
                    service: service.to_string(),
                    user: user.to_string(),
                    secret: self.secret.clone(),
                },
            )))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Debug)]
    struct DeleteFailureCredential {
        service: String,
        user: String,
        secret: Vec<u8>,
    }

    impl CredentialApi for DeleteFailureCredential {
        fn set_secret(&self, _secret: &[u8]) -> Result<()> {
            Ok(())
        }

        fn get_secret(&self) -> Result<Vec<u8>> {
            if self.secret.is_empty() {
                return Err(Error::Invalid(
                    "legacy get".to_string(),
                    "failed".to_string(),
                ));
            }

            Ok(self.secret.clone())
        }

        fn delete_credential(&self) -> Result<()> {
            Err(Error::Invalid(
                "legacy delete".to_string(),
                "failed".to_string(),
            ))
        }

        fn get_credential(&self) -> Result<Option<Arc<Credential>>> {
            Ok(None)
        }

        fn get_specifiers(&self) -> Option<(String, String)> {
            Some((self.service.clone(), self.user.clone()))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn owned_map(modifiers: Option<&HashMap<&str, &str>>) -> HashMap<String, String> {
        modifiers
            .into_iter()
            .flat_map(HashMap::iter)
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn build_injects_default_target() {
        let inner = Arc::new(RecordingStore::default());
        let store = TargetedCredentialStore::new(inner.clone(), LINUX_SECRET_SERVICE_TARGET);

        store.build("service", "user", None).unwrap();

        assert_eq!(
            inner.last_build_modifier("target"),
            Some(LINUX_SECRET_SERVICE_TARGET.to_string())
        );
    }

    #[test]
    fn targeted_store_delegates_metadata_to_inner_store() {
        let inner = Arc::new(RecordingStore::default());
        assert!(format!("{inner:?}").contains("RecordingStore"));
        let store: Arc<CredentialStore> =
            TargetedCredentialStore::new(inner, LINUX_SECRET_SERVICE_TARGET);

        assert_eq!(store.vendor(), "test-store");
        assert_eq!(store.id(), "test-store");
        assert!(matches!(
            store.persistence(),
            CredentialPersistence::UntilDelete
        ));
        assert!(store.as_any().is::<TargetedCredentialStore>());
        assert!(format!("{store:?}").contains("TargetedCredentialStore"));
    }

    #[test]
    fn build_preserves_explicit_target() {
        let inner = Arc::new(RecordingStore::default());
        let store = TargetedCredentialStore::new(inner.clone(), LINUX_SECRET_SERVICE_TARGET);
        let modifiers = HashMap::from([("target", "explicit")]);

        store.build("service", "user", Some(&modifiers)).unwrap();

        assert_eq!(
            inner.last_build_modifier("target"),
            Some("explicit".to_string())
        );
    }

    #[test]
    fn search_injects_default_target() {
        let inner = Arc::new(RecordingStore::default());
        let store = TargetedCredentialStore::new(inner.clone(), LINUX_SECRET_SERVICE_TARGET);
        let spec = HashMap::from([("service", "service"), ("user", "user")]);

        store.search(&spec).unwrap();

        assert_eq!(
            inner.last_search_spec("target"),
            Some(LINUX_SECRET_SERVICE_TARGET.to_string())
        );
    }

    #[test]
    fn search_preserves_explicit_target() {
        let inner = Arc::new(RecordingStore::default());
        let store = TargetedCredentialStore::new(inner.clone(), LINUX_SECRET_SERVICE_TARGET);
        let spec = HashMap::from([("service", "service"), ("target", "explicit")]);

        store.search(&spec).unwrap();

        assert_eq!(
            inner.last_search_spec("target"),
            Some("explicit".to_string())
        );
    }

    #[test]
    fn target_is_non_default_for_wsl() {
        assert_eq!(LINUX_SECRET_SERVICE_TARGET, "whitenoise");
        assert_ne!(LINUX_SECRET_SERVICE_TARGET, "default");
    }

    #[test]
    fn targeted_primary_store_migrates_legacy_secret() {
        let primary_inner = Arc::new(RecordingStore::default());
        let primary: Arc<CredentialStore> =
            TargetedCredentialStore::new(primary_inner.clone(), LINUX_SECRET_SERVICE_TARGET);
        let legacy = keyring_core::mock::Store::new().unwrap();
        let legacy_entry = legacy
            .build("com.whitenoise.app", "mdk.db.key.pubkey", None)
            .unwrap();
        legacy_entry.set_secret(b"legacy-mdk-key").unwrap();
        let store = LegacyMigrationCredentialStore::new(primary, legacy);

        let entry = store
            .build("com.whitenoise.app", "mdk.db.key.pubkey", None)
            .unwrap();

        assert_eq!(entry.get_secret().unwrap(), b"legacy-mdk-key");
        assert_eq!(
            primary_inner.last_build_modifier("target"),
            Some(LINUX_SECRET_SERVICE_TARGET.to_string())
        );
        assert!(matches!(legacy_entry.get_secret(), Err(Error::NoEntry)));
    }

    #[test]
    fn legacy_migration_store_delegates_metadata_and_search_to_primary() {
        let primary = keyring_core::mock::Store::new().unwrap();
        let legacy = keyring_core::mock::Store::new().unwrap();
        let store: Arc<CredentialStore> = LegacyMigrationCredentialStore::new(primary, legacy);
        let spec = HashMap::from([("service", "com.whitenoise.app")]);

        assert!(store.vendor().contains("with legacy migration from"));
        assert!(store.id().contains("with legacy migration"));
        assert!(matches!(
            store.persistence(),
            CredentialPersistence::ProcessOnly
        ));
        assert!(store.as_any().is::<LegacyMigrationCredentialStore>());
        assert!(format!("{store:?}").contains("LegacyMigrationCredentialStore"));
        assert!(store.search(&spec).unwrap().is_empty());
    }

    #[test]
    fn legacy_migration_store_migrates_legacy_secret_to_primary_and_removes_legacy() {
        let primary = keyring_core::mock::Store::new().unwrap();
        let legacy = keyring_core::mock::Store::new().unwrap();
        let legacy_entry = legacy
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();
        legacy_entry.set_secret(b"legacy-db-key").unwrap();
        let store = LegacyMigrationCredentialStore::new(primary.clone(), legacy.clone());

        let entry = store
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();

        assert_eq!(entry.get_secret().unwrap(), b"legacy-db-key");
        assert_eq!(
            primary
                .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
                .unwrap()
                .get_secret()
                .unwrap(),
            b"legacy-db-key"
        );
        assert!(matches!(legacy_entry.get_secret(), Err(Error::NoEntry)));
    }

    #[test]
    fn legacy_migration_store_reads_primary_when_legacy_secret_is_absent() {
        let primary = keyring_core::mock::Store::new().unwrap();
        let legacy = keyring_core::mock::Store::new().unwrap();
        primary
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap()
            .set_secret(b"primary-db-key")
            .unwrap();
        let store = LegacyMigrationCredentialStore::new(primary, legacy);

        let entry = store
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();

        assert_eq!(entry.get_secret().unwrap(), b"primary-db-key");
    }

    #[test]
    fn legacy_migration_store_sets_primary_secret() {
        let primary = keyring_core::mock::Store::new().unwrap();
        let legacy = keyring_core::mock::Store::new().unwrap();
        let store = LegacyMigrationCredentialStore::new(primary.clone(), legacy);

        let entry = store
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();
        entry.set_secret(b"new-db-key").unwrap();

        assert_eq!(
            primary
                .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
                .unwrap()
                .get_secret()
                .unwrap(),
            b"new-db-key"
        );
    }

    #[test]
    fn legacy_migration_store_set_secret_removes_stale_legacy_secret() {
        let primary = keyring_core::mock::Store::new().unwrap();
        let legacy = keyring_core::mock::Store::new().unwrap();
        let legacy_entry = legacy
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();
        legacy_entry.set_secret(b"stale-legacy-db-key").unwrap();
        let store = LegacyMigrationCredentialStore::new(primary.clone(), legacy);

        let entry = store
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();
        entry.set_secret(b"new-primary-db-key").unwrap();

        assert_eq!(entry.get_secret().unwrap(), b"new-primary-db-key");
        assert_eq!(
            primary
                .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
                .unwrap()
                .get_secret()
                .unwrap(),
            b"new-primary-db-key"
        );
        assert!(matches!(legacy_entry.get_secret(), Err(Error::NoEntry)));
    }

    #[test]
    fn legacy_migration_store_get_credential_does_not_migrate_legacy_secret() {
        let primary = keyring_core::mock::Store::new().unwrap();
        let legacy = keyring_core::mock::Store::new().unwrap();
        let primary_entry = primary
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();
        let legacy_entry = legacy
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();
        legacy_entry.set_secret(b"legacy-db-key").unwrap();
        let store = LegacyMigrationCredentialStore::new(primary, legacy);

        let entry = store
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();
        let credential = entry.get_credential().unwrap();

        assert!(matches!(primary_entry.get_secret(), Err(Error::NoEntry)));
        assert_eq!(legacy_entry.get_secret().unwrap(), b"legacy-db-key");
        assert_eq!(credential.get_secret().unwrap(), b"legacy-db-key");
        assert_eq!(
            primary_entry.get_secret().unwrap(),
            b"legacy-db-key",
            "migration should happen on the explicit secret read"
        );
        assert!(matches!(legacy_entry.get_secret(), Err(Error::NoEntry)));
    }

    #[test]
    fn legacy_migration_store_returns_legacy_get_error() {
        let primary = keyring_core::mock::Store::new().unwrap();
        let legacy = Arc::new(DeleteFailureStore { secret: Vec::new() });
        let store = LegacyMigrationCredentialStore::new(primary, legacy);

        let entry = store
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();
        let result = entry.get_secret();

        assert!(matches!(result, Err(Error::Invalid(field, _)) if field == "legacy get"));
    }

    #[test]
    fn legacy_migration_credential_exposes_specifiers_and_debug() {
        let primary = keyring_core::mock::Store::new().unwrap();
        let legacy = keyring_core::mock::Store::new().unwrap();
        primary
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap()
            .set_secret(b"primary-db-key")
            .unwrap();
        let store = LegacyMigrationCredentialStore::new(primary, legacy);

        let entry = store
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();

        assert_eq!(
            entry.get_credential().unwrap().get_secret().unwrap(),
            b"primary-db-key"
        );
        assert_eq!(
            entry.get_specifiers(),
            Some((
                "com.whitenoise.app".to_string(),
                "whitenoise.db.key.v1".to_string()
            ))
        );
        assert!(entry.as_any().is::<LegacyMigrationCredential>());
        assert!(format!("{entry:?}").contains("LegacyMigrationCredential"));
    }

    #[test]
    fn legacy_migration_store_deletes_primary_and_legacy_entries() {
        let primary = keyring_core::mock::Store::new().unwrap();
        let legacy = keyring_core::mock::Store::new().unwrap();
        let primary_entry = primary
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();
        let legacy_entry = legacy
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();
        primary_entry.set_secret(b"primary-db-key").unwrap();
        legacy_entry.set_secret(b"legacy-db-key").unwrap();
        let store = LegacyMigrationCredentialStore::new(primary, legacy);

        let entry = store
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();

        entry.delete_credential().unwrap();
        assert!(matches!(primary_entry.get_secret(), Err(Error::NoEntry)));
        assert!(matches!(legacy_entry.get_secret(), Err(Error::NoEntry)));
    }

    #[test]
    fn legacy_migration_store_delete_returns_no_entry_when_both_stores_are_empty() {
        let primary = keyring_core::mock::Store::new().unwrap();
        let legacy = keyring_core::mock::Store::new().unwrap();
        let store = LegacyMigrationCredentialStore::new(primary, legacy);

        let entry = store
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();
        let result = entry.delete_credential();

        assert!(matches!(result, Err(Error::NoEntry)));
    }

    #[test]
    fn legacy_migration_store_delete_returns_error_when_legacy_delete_fails() {
        let primary = keyring_core::mock::Store::new().unwrap();
        let legacy = Arc::new(DeleteFailureStore {
            secret: b"legacy-db-key".to_vec(),
        });
        let store = LegacyMigrationCredentialStore::new(primary.clone(), legacy);
        let primary_entry = primary
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();
        primary_entry.set_secret(b"primary-db-key").unwrap();

        let entry = store
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();
        let result = entry.delete_credential();

        assert!(matches!(result, Err(Error::Invalid(field, _)) if field == "legacy delete"));
        assert!(matches!(primary_entry.get_secret(), Err(Error::NoEntry)));
    }

    #[test]
    fn legacy_migration_store_returns_error_when_legacy_delete_fails() {
        let primary = keyring_core::mock::Store::new().unwrap();
        let legacy_secret = b"legacy-db-key".to_vec();
        let legacy = Arc::new(DeleteFailureStore {
            secret: legacy_secret.clone(),
        });
        let store = LegacyMigrationCredentialStore::new(primary.clone(), legacy);

        let entry = store
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();
        let result = entry.get_secret();

        assert!(matches!(result, Err(Error::Invalid(field, _)) if field == "legacy delete"));
        assert_eq!(
            primary
                .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
                .unwrap()
                .get_secret()
                .unwrap(),
            legacy_secret
        );
    }

    #[test]
    fn test_credential_methods_are_wired_for_store_boundaries() {
        let credential = TestCredential;

        credential.set_secret(b"secret").unwrap();
        assert!(matches!(credential.get_secret(), Err(Error::NoEntry)));
        credential.delete_credential().unwrap();
        assert!(credential.get_credential().unwrap().is_none());
        assert_eq!(credential.get_specifiers(), None);
        assert!(credential.as_any().is::<TestCredential>());
        assert!(format!("{credential:?}").contains("TestCredential"));
    }

    #[test]
    fn delete_failure_store_reports_metadata_and_specifiers() {
        let store = DeleteFailureStore {
            secret: b"legacy-db-key".to_vec(),
        };

        assert_eq!(store.vendor(), "delete-failure-store");
        assert_eq!(store.id(), "delete-failure-store");
        assert!(store.as_any().is::<DeleteFailureStore>());
        assert!(format!("{store:?}").contains("DeleteFailureStore"));

        let entry = store
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap();

        entry.set_secret(b"ignored").unwrap();
        assert_eq!(entry.get_secret().unwrap(), b"legacy-db-key");
        assert_eq!(
            entry.get_credential().unwrap().get_secret().unwrap(),
            b"legacy-db-key"
        );
        assert_eq!(
            entry.get_specifiers(),
            Some((
                "com.whitenoise.app".to_string(),
                "whitenoise.db.key.v1".to_string()
            ))
        );
        assert!(entry.as_any().is::<DeleteFailureCredential>());

        let credential = DeleteFailureCredential {
            service: "com.whitenoise.app".to_string(),
            user: "whitenoise.db.key.v1".to_string(),
            secret: b"legacy-db-key".to_vec(),
        };
        assert!(format!("{credential:?}").contains("DeleteFailureCredential"));
    }
}

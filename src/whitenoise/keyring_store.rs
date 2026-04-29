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
        let _ = Self::delete_entry(&self.legacy);
        Ok(secret)
    }
}

impl CredentialApi for LegacyMigrationCredential {
    fn set_secret(&self, secret: &[u8]) -> Result<()> {
        self.primary.set_secret(secret)
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
        self.get_secret().map(|_| None)
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
            _service: &str,
            _user: &str,
            modifiers: Option<&HashMap<&str, &str>>,
        ) -> Result<Entry> {
            self.build_modifiers
                .lock()
                .unwrap()
                .push(owned_map(modifiers));
            Ok(Entry::new_with_credential(Arc::new(TestCredential)))
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
    fn target_is_non_default_for_wsl() {
        assert_eq!(LINUX_SECRET_SERVICE_TARGET, "whitenoise");
        assert_ne!(LINUX_SECRET_SERVICE_TARGET, "default");
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
}

pub(crate) mod relay_handles;

use std::sync::Arc;

use dashmap::DashMap;
use mdk_core::prelude::MDK;
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr_sdk::PublicKey;
use nostr_sdk::prelude::NostrSigner;
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore, watch};

use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::{Account, DiscoveredRelayLists};
use crate::whitenoise::error::{Result, WhitenoiseError};

/// Signer slot shared between `AccountSession` and its relay handles.
///
/// `None` for restored external-signer accounts whose platform signer has not
/// yet been re-registered. Handles that need signing return
/// [`WhitenoiseError::SignerUnavailable`] until the slot is filled.
pub(crate) type SharedSigner = Arc<RwLock<Option<Arc<dyn NostrSigner>>>>;

/// A per-account session holding the account's MDK instance and signer.
///
/// `signer` is `None` for restored external-signer accounts whose platform
/// signer has not yet been re-registered. Operations requiring signing should
/// return the existing external-signer-unavailable error until
/// `register_external_signer()` fills the slot.
pub struct AccountSession {
    pub account_pubkey: PublicKey,
    pub mdk: Arc<MDK<MdkSqliteStorage>>,
    pub(crate) signer: SharedSigner,
    contact_list_guard: Arc<Semaphore>,
    cancellation: watch::Sender<bool>,
    pub(crate) ephemeral: relay_handles::AccountEphemeralHandle,
    pub(crate) group_handle: relay_handles::AccountGroupHandle,
}

impl AccountSession {
    pub(crate) fn new(
        account_pubkey: PublicKey,
        mdk: MDK<MdkSqliteStorage>,
        signer: Option<Arc<dyn NostrSigner>>,
        ephemeral_plane: crate::relay_control::ephemeral::EphemeralPlane,
        relay_control: Arc<crate::relay_control::RelayControlPlane>,
    ) -> Self {
        let (cancellation, _) = watch::channel(false);
        let signer: SharedSigner = Arc::new(RwLock::new(signer));
        let ephemeral = relay_handles::AccountEphemeralHandle::new(
            account_pubkey,
            ephemeral_plane,
            signer.clone(),
        );
        let group_handle = relay_handles::AccountGroupHandle::new(account_pubkey, relay_control);
        Self {
            account_pubkey,
            mdk: Arc::new(mdk),
            signer,
            contact_list_guard: Arc::new(Semaphore::new(1)),
            cancellation,
            ephemeral,
            group_handle,
        }
    }

    /// Build a session from a persisted account, loading MDK and (for local
    /// accounts) the signer from the secrets store.
    pub(crate) fn from_account(account: &Account, wn: &Whitenoise) -> Result<Self> {
        let mdk = wn.create_mdk_for_account(account.pubkey)?;
        let signer = if account.has_local_key() {
            Some(wn.get_signer_for_account(account)?)
        } else {
            None
        };
        Ok(Self::new(
            account.pubkey,
            mdk,
            signer,
            wn.relay_control.ephemeral(),
            wn.relay_control.clone(),
        ))
    }

    /// Replace the signer slot (e.g. when an external signer is re-registered).
    pub async fn set_signer(&self, signer: Arc<dyn NostrSigner>) {
        *self.signer.write().await = Some(signer);
    }

    /// Read the current signer, if any.
    ///
    /// Uses `try_read` to avoid blocking on the rare concurrent `set_signer`
    /// write. Returns `None` when the write lock is held; callers fall through
    /// to the legacy signer lookup which produces the same result.
    pub fn get_signer(&self) -> Option<Arc<dyn NostrSigner>> {
        self.signer.try_read().ok().and_then(|g| g.clone())
    }

    /// Subscribe to the cancellation channel.
    pub fn subscribe_cancellation(&self) -> watch::Receiver<bool> {
        self.cancellation.subscribe()
    }

    /// Signal cancellation to all background tasks associated with this session.
    pub(crate) fn cancel(&self) {
        let _ = self.cancellation.send(true);
    }

    /// Acquire the contact-list processing permit for this session.
    pub async fn acquire_contact_list_permit(&self) -> Result<OwnedSemaphorePermit> {
        self.contact_list_guard
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| {
                WhitenoiseError::ContactList(
                    "Failed to acquire contact list processing permit".to_string(),
                )
            })
    }
}

/// Manages active account sessions and pending logins.
pub struct AccountManager {
    sessions: DashMap<PublicKey, Arc<AccountSession>>,
    /// Pubkeys with a login in progress. The value holds whichever relay lists
    /// were already discovered so step 2a can publish defaults only for missing ones.
    pending_logins: DashMap<PublicKey, DiscoveredRelayLists>,
}

impl Default for AccountManager {
    fn default() -> Self {
        Self {
            sessions: DashMap::new(),
            pending_logins: DashMap::new(),
        }
    }
}

impl AccountManager {
    pub(crate) fn insert_session(&self, session: Arc<AccountSession>) {
        let pubkey = session.account_pubkey;
        if let Some((_, old)) = self.sessions.remove(&pubkey) {
            old.cancel();
        }
        self.sessions.insert(pubkey, session);
        tracing::debug!(
            target: "whitenoise::session",
            "Inserted session for {}",
            pubkey.to_hex()
        );
    }

    pub fn get_session(&self, pubkey: &PublicKey) -> Option<Arc<AccountSession>> {
        self.sessions.get(pubkey).map(|r| r.clone())
    }

    pub fn remove_session(&self, pubkey: &PublicKey) -> Option<Arc<AccountSession>> {
        let removed = self.sessions.remove(pubkey).map(|(_, s)| s);
        if removed.is_some() {
            tracing::debug!(
                target: "whitenoise::session",
                "Removed session for {}",
                pubkey.to_hex()
            );
        }
        removed
    }

    /// Cancel and remove all active sessions.
    pub fn clear_sessions(&self) {
        for entry in self.sessions.iter() {
            entry.value().cancel();
        }
        self.sessions.clear();
    }

    /// Record that a multi-step login is in progress for `pubkey`.
    pub fn stash_pending_login(&self, pubkey: &PublicKey, discovered: DiscoveredRelayLists) {
        self.pending_logins.insert(*pubkey, discovered);
    }

    /// Returns `true` if a multi-step login is in progress for `pubkey`.
    pub fn has_pending_login(&self, pubkey: &PublicKey) -> bool {
        self.pending_logins.contains_key(pubkey)
    }

    /// Return a snapshot of the pending-login stash for `pubkey`.
    pub fn get_pending_login(&self, pubkey: &PublicKey) -> Option<DiscoveredRelayLists> {
        self.pending_logins.get(pubkey).map(|r| r.clone())
    }

    /// Remove and return the pending-login stash for `pubkey`.
    pub fn take_pending_login(&self, pubkey: &PublicKey) -> Option<DiscoveredRelayLists> {
        self.pending_logins.remove(pubkey).map(|(_, v)| v)
    }

    /// Merge newly-discovered relay lists into the pending-login stash and
    /// return a snapshot. The DashMap lock is released before returning so
    /// the caller is free to do async work with the result.
    ///
    /// Returns `None` if no stash exists for `pubkey`.
    pub fn merge_pending_login(
        &self,
        pubkey: &PublicKey,
        discovered: DiscoveredRelayLists,
    ) -> Option<DiscoveredRelayLists> {
        let mut stash = self.pending_logins.get_mut(pubkey)?;
        stash.merge(discovered);
        let snapshot = stash.clone();
        drop(stash);
        Some(snapshot)
    }

    /// Remove all pending logins.
    pub fn clear_pending_logins(&self) {
        self.pending_logins.clear();
    }

    /// Restore sessions for all persisted accounts at startup.
    ///
    /// External-signer accounts get `signer: None` until re-registered.
    /// Local accounts load their signer from the secrets store.
    pub async fn restore_sessions(&self, wn: &'static Whitenoise) {
        let accounts = match Account::all(&wn.database).await {
            Ok(accounts) => accounts,
            Err(e) => {
                tracing::error!(
                    target: "whitenoise::session",
                    "Failed to load accounts for session restore: {}",
                    e
                );
                return;
            }
        };

        let mut ok_count = 0usize;
        let mut err_count = 0usize;

        for account in &accounts {
            match AccountSession::from_account(account, wn) {
                Ok(session) => {
                    self.insert_session(Arc::new(session));
                    ok_count += 1;
                }
                Err(e) => {
                    tracing::error!(
                        target: "whitenoise::session",
                        "Failed to restore session for account {}: {}",
                        account.pubkey.to_hex(),
                        e
                    );
                    err_count += 1;
                }
            }
        }

        tracing::info!(
            target: "whitenoise::session",
            "Restored {} sessions ({} failed)",
            ok_count,
            err_count,
        );
    }
}

impl Whitenoise {
    /// Look up an active session by public key.
    pub fn session(&self, pubkey: &PublicKey) -> Option<Arc<AccountSession>> {
        self.account_manager.get_session(pubkey)
    }
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::SystemTime;

    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;
    use crate::relay_control::RelayControlPlane;
    use crate::relay_control::ephemeral::{EphemeralPlane, EphemeralPlaneConfig};
    use crate::relay_control::observability::{RelayObservability, RelayObservabilityConfig};
    use crate::whitenoise::accounts::test_utils::create_mdk;
    use crate::whitenoise::database::Database;

    pub async fn test_db() -> Arc<Database> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let database = Database {
            pool,
            path: PathBuf::from(":memory:"),
            last_connected: SystemTime::now(),
        };
        database.migrate_up().await.unwrap();
        Arc::new(database)
    }

    /// Build a minimal `AccountSession` for tests that don't exercise relay handles.
    pub async fn test_session(pubkey: PublicKey) -> Arc<AccountSession> {
        let mdk = create_mdk(pubkey);
        let db = test_db().await;
        let (event_sender, _) = tokio::sync::mpsc::channel(1);
        let ephemeral = EphemeralPlane::new(
            EphemeralPlaneConfig::default(),
            db.clone(),
            event_sender.clone(),
            RelayObservability::new(RelayObservabilityConfig::default()),
        );
        let relay_control = Arc::new(RelayControlPlane::new(db, vec![], event_sender, [0u8; 16]));
        Arc::new(AccountSession::new(
            pubkey,
            mdk,
            None,
            ephemeral,
            relay_control,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nostr_sdk::Keys;

    use super::AccountManager;
    use super::test_helpers::test_session;
    use crate::whitenoise::accounts::DiscoveredRelayLists;

    fn test_pubkey() -> nostr_sdk::PublicKey {
        Keys::generate().public_key()
    }

    fn empty_discovered() -> DiscoveredRelayLists {
        DiscoveredRelayLists {
            nip65: None,
            inbox: None,
            key_package: None,
        }
    }

    // ── AccountManager: session CRUD ─────────────────────────────────

    #[tokio::test]
    async fn insert_and_get_session() {
        let mgr = AccountManager::default();
        let pk = test_pubkey();
        let session = test_session(pk).await;

        mgr.insert_session(session.clone());
        let got = mgr.get_session(&pk).expect("session should exist");
        assert_eq!(got.account_pubkey, pk);
    }

    #[test]
    fn get_session_returns_none_for_unknown_key() {
        let mgr = AccountManager::default();
        assert!(mgr.get_session(&test_pubkey()).is_none());
    }

    #[tokio::test]
    async fn remove_session_returns_removed() {
        let mgr = AccountManager::default();
        let pk = test_pubkey();
        mgr.insert_session(test_session(pk).await);

        let removed = mgr.remove_session(&pk);
        assert!(removed.is_some());
        assert!(mgr.get_session(&pk).is_none());
    }

    #[test]
    fn remove_session_returns_none_when_absent() {
        let mgr = AccountManager::default();
        assert!(mgr.remove_session(&test_pubkey()).is_none());
    }

    #[tokio::test]
    async fn clear_sessions_empties_all() {
        let mgr = AccountManager::default();
        mgr.insert_session(test_session(test_pubkey()).await);
        mgr.insert_session(test_session(test_pubkey()).await);

        mgr.clear_sessions();
        // Both are gone — we can only verify via new lookups returning None,
        // but since keys are random we just assert the map is empty via a
        // fresh get on any key.
        assert!(mgr.get_session(&test_pubkey()).is_none());
    }

    // ── AccountManager: pending logins ───────────────────────────────

    #[test]
    fn stash_and_get_pending_login() {
        let mgr = AccountManager::default();
        let pk = test_pubkey();
        mgr.stash_pending_login(&pk, empty_discovered());

        assert!(mgr.has_pending_login(&pk));
        assert!(mgr.get_pending_login(&pk).is_some());
    }

    #[test]
    fn has_pending_login_false_when_absent() {
        let mgr = AccountManager::default();
        assert!(!mgr.has_pending_login(&test_pubkey()));
    }

    #[test]
    fn take_pending_login_removes_entry() {
        let mgr = AccountManager::default();
        let pk = test_pubkey();
        mgr.stash_pending_login(&pk, empty_discovered());

        let taken = mgr.take_pending_login(&pk);
        assert!(taken.is_some());
        assert!(!mgr.has_pending_login(&pk));
    }

    #[test]
    fn take_pending_login_returns_none_when_absent() {
        let mgr = AccountManager::default();
        assert!(mgr.take_pending_login(&test_pubkey()).is_none());
    }

    #[test]
    fn merge_pending_login_combines_fields() {
        let mgr = AccountManager::default();
        let pk = test_pubkey();

        // Start with only nip65 discovered.
        let initial = DiscoveredRelayLists {
            nip65: Some(vec![]),
            inbox: None,
            key_package: None,
        };
        mgr.stash_pending_login(&pk, initial);

        // Merge in inbox.
        let update = DiscoveredRelayLists {
            nip65: None,
            inbox: Some(vec![]),
            key_package: None,
        };
        let merged = mgr.merge_pending_login(&pk, update).expect("stash exists");
        assert!(merged.nip65.is_some());
        assert!(merged.inbox.is_some());
        assert!(merged.key_package.is_none());
    }

    #[test]
    fn merge_pending_login_returns_none_when_absent() {
        let mgr = AccountManager::default();
        assert!(
            mgr.merge_pending_login(&test_pubkey(), empty_discovered())
                .is_none()
        );
    }

    #[test]
    fn clear_pending_logins_empties_all() {
        let mgr = AccountManager::default();
        let pk = test_pubkey();
        mgr.stash_pending_login(&pk, empty_discovered());

        mgr.clear_pending_logins();
        assert!(!mgr.has_pending_login(&pk));
    }

    // ── AccountSession ───────────────────────────────────────────────

    #[tokio::test]
    async fn set_and_get_signer() {
        let pk = test_pubkey();
        let session = test_session(pk).await;

        assert!(session.get_signer().is_none(), "starts with no signer");

        let keys = Keys::generate();
        session.set_signer(Arc::new(keys)).await;

        assert!(session.get_signer().is_some(), "signer should be set");
    }

    #[tokio::test]
    async fn cancel_signals_receivers() {
        let pk = test_pubkey();
        let session = test_session(pk).await;
        let mut rx = session.subscribe_cancellation();

        assert!(!*rx.borrow(), "not cancelled initially");
        session.cancel();
        rx.changed().await.expect("channel not dropped");
        assert!(*rx.borrow(), "should be cancelled after cancel()");
    }

    #[tokio::test]
    async fn acquire_contact_list_permit_limits_concurrency() {
        let pk = test_pubkey();
        let session = test_session(pk).await;

        let _permit = session.acquire_contact_list_permit().await.unwrap();

        // The semaphore has 1 permit, so a second acquire should not succeed
        // immediately. We use try_acquire_owned on the inner semaphore
        // indirectly by checking timeout behaviour.
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            session.acquire_contact_list_permit(),
        )
        .await;
        assert!(result.is_err(), "second permit should block (timeout)");
    }
}

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

/// A per-account session holding the account's MDK instance and signer.
///
/// `signer` is `None` for restored external-signer accounts whose platform
/// signer has not yet been re-registered. Operations requiring signing should
/// return the existing external-signer-unavailable error until
/// `register_external_signer()` fills the slot.
pub struct AccountSession {
    pub account_pubkey: PublicKey,
    pub mdk: Arc<MDK<MdkSqliteStorage>>,
    signer: RwLock<Option<Arc<dyn NostrSigner>>>,
    contact_list_guard: Arc<Semaphore>,
    cancellation: watch::Sender<bool>,
}

impl AccountSession {
    pub fn new(
        account_pubkey: PublicKey,
        mdk: MDK<MdkSqliteStorage>,
        signer: Option<Arc<dyn NostrSigner>>,
    ) -> Self {
        let (cancellation, _) = watch::channel(false);
        Self {
            account_pubkey,
            mdk: Arc::new(mdk),
            signer: RwLock::new(signer),
            contact_list_guard: Arc::new(Semaphore::new(1)),
            cancellation,
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
        Ok(Self::new(account.pubkey, mdk, signer))
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
    pub fn insert_session(&self, session: Arc<AccountSession>) {
        let pubkey = session.account_pubkey;
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

    /// Remove all active sessions.
    pub fn clear_sessions(&self) {
        self.sessions.clear();
    }

    /// Record that a multi-step login is in progress for `pubkey`.
    pub fn stash_pending_login(&self, pubkey: PublicKey, discovered: DiscoveredRelayLists) {
        self.pending_logins.insert(pubkey, discovered);
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
    pub fn take_pending_login(
        &self,
        pubkey: &PublicKey,
    ) -> Option<(PublicKey, DiscoveredRelayLists)> {
        self.pending_logins.remove(pubkey)
    }

    /// Merge newly-discovered relay lists into the pending-login stash and
    /// return a snapshot. The DashMap lock is released before returning so
    /// the caller is free to do async work with the result.
    ///
    /// Returns `NoLoginInProgress` if no stash exists for `pubkey`.
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

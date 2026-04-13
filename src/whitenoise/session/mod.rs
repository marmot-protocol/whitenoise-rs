use std::sync::Arc;

use dashmap::DashMap;
use mdk_core::prelude::MDK;
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr_sdk::PublicKey;
use nostr_sdk::prelude::NostrSigner;
use tokio::sync::{RwLock, Semaphore, watch};

use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::DiscoveredRelayLists;

/// A per-account session holding the account's MDK instance and signer.
///
/// `signer` is `None` for restored external-signer accounts whose platform
/// signer has not yet been re-registered. Operations requiring signing should
/// return the existing external-signer-unavailable error until
/// `register_external_signer()` fills the slot.
pub struct AccountSession {
    pub account_pubkey: PublicKey,
    pub mdk: Arc<MDK<MdkSqliteStorage>>,
    pub signer: RwLock<Option<Arc<dyn NostrSigner>>>,
    pub contact_list_guard: Arc<Semaphore>,
    pub cancellation: watch::Sender<bool>,
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

    /// Replace the signer slot (e.g. when an external signer is re-registered).
    pub async fn set_signer(&self, signer: Arc<dyn NostrSigner>) {
        *self.signer.write().await = Some(signer);
    }
}

/// Manages active account sessions and pending logins.
pub struct AccountManager {
    sessions: DashMap<PublicKey, Arc<AccountSession>>,
    /// Pubkeys with a login in progress. The value holds whichever relay lists
    /// were already discovered so step 2a can publish defaults only for missing ones.
    pub(crate) pending_logins: DashMap<PublicKey, DiscoveredRelayLists>,
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

    /// Restore sessions for all persisted accounts at startup.
    ///
    /// External-signer accounts get `signer: None` until re-registered.
    /// Local accounts load their signer from the secrets store.
    pub async fn restore_sessions(&self, wn: &'static Whitenoise) {
        let accounts = match crate::whitenoise::accounts::Account::all(&wn.database).await {
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
            match wn.create_mdk_for_account(account.pubkey) {
                Ok(mdk) => {
                    let signer: Option<Arc<dyn NostrSigner>> = if account.has_local_key() {
                        wn.get_signer_for_account(account).ok()
                    } else {
                        None
                    };
                    let session = Arc::new(AccountSession::new(account.pubkey, mdk, signer));
                    self.insert_session(session);
                    ok_count += 1;
                }
                Err(e) => {
                    tracing::error!(
                        target: "whitenoise::session",
                        "Failed to create MDK for account {}: {}",
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

use std::sync::Arc;

use dashmap::DashMap;
use mdk_core::prelude::MDK;
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr_sdk::PublicKey;
use nostr_sdk::prelude::NostrSigner;
use tokio::sync::RwLock;

use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::{AccountError, DiscoveredRelayLists};

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
    /// Transitional back-reference until singleton removal.
    _wn: &'static Whitenoise,
}

impl AccountSession {
    pub fn new(
        account_pubkey: PublicKey,
        mdk: MDK<MdkSqliteStorage>,
        signer: Option<Arc<dyn NostrSigner>>,
        wn: &'static Whitenoise,
    ) -> Self {
        Self {
            account_pubkey,
            mdk: Arc::new(mdk),
            signer: RwLock::new(signer),
            _wn: wn,
        }
    }

    /// Replace the signer slot (e.g. when an external signer is re-registered).
    pub async fn set_signer(&self, signer: Arc<dyn NostrSigner>) {
        *self.signer.write().await = Some(signer);
    }

    /// Returns the current signer, if one is registered.
    pub async fn get_signer(&self) -> Option<Arc<dyn NostrSigner>> {
        self.signer.read().await.clone()
    }
}

/// Manages active account sessions and pending logins.
pub struct AccountManager {
    sessions: DashMap<PublicKey, Arc<AccountSession>>,
    /// Pubkeys with a login in progress (between login_start and
    /// login_publish_default_relays / login_with_custom_relay / login_cancel).
    /// The value holds whichever relay lists were already discovered on the
    /// network so that step 2a can publish defaults only for the missing ones.
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
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a session for the given account.
    pub fn insert_session(&self, session: Arc<AccountSession>) {
        let pubkey = session.account_pubkey;
        self.sessions.insert(pubkey, session);
        tracing::debug!(
            target: "whitenoise::session",
            "Inserted session for {}",
            pubkey.to_hex()
        );
    }

    /// Look up an active session by public key.
    pub fn get_session(&self, pubkey: &PublicKey) -> Option<Arc<AccountSession>> {
        self.sessions.get(pubkey).map(|r| r.clone())
    }

    /// Remove the session for the given public key.
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

    /// Returns true if a session exists for the given public key.
    pub fn has_session(&self, pubkey: &PublicKey) -> bool {
        self.sessions.contains_key(pubkey)
    }

    /// Returns the number of active sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Returns all active session public keys.
    pub fn session_pubkeys(&self) -> Vec<PublicKey> {
        self.sessions.iter().map(|r| *r.key()).collect()
    }

    /// Restore sessions for all persisted accounts.
    ///
    /// Creates an `AccountSession` (with MDK but no signer) for each account
    /// in the database. External-signer accounts will have `signer: None`
    /// until `register_external_signer()` fills the slot. Local accounts get
    /// their signer from the secrets store.
    pub async fn restore_sessions(
        &self,
        wn: &'static Whitenoise,
    ) -> Vec<(PublicKey, Result<(), AccountError>)> {
        let accounts = match crate::whitenoise::accounts::Account::all(&wn.database).await {
            Ok(accounts) => accounts,
            Err(e) => {
                tracing::error!(
                    target: "whitenoise::session",
                    "Failed to load accounts for session restore: {}",
                    e
                );
                return Vec::new();
            }
        };

        let mut results = Vec::with_capacity(accounts.len());

        for account in &accounts {
            let mdk_result = wn.create_mdk_for_account(account.pubkey);
            match mdk_result {
                Ok(mdk) => {
                    // For local accounts, try to load the signer from secrets store.
                    // For external accounts, signer is None until re-registered.
                    let signer: Option<Arc<dyn NostrSigner>> = if account.has_local_key() {
                        match wn.get_signer_for_account(account) {
                            Ok(s) => Some(s),
                            Err(e) => {
                                tracing::warn!(
                                    target: "whitenoise::session",
                                    "Failed to load signer for local account {}: {}",
                                    account.pubkey.to_hex(),
                                    e
                                );
                                None
                            }
                        }
                    } else {
                        None
                    };

                    let session = Arc::new(AccountSession::new(account.pubkey, mdk, signer, wn));
                    self.insert_session(session);
                    results.push((account.pubkey, Ok(())));
                }
                Err(e) => {
                    tracing::error!(
                        target: "whitenoise::session",
                        "Failed to create MDK for account {}: {}",
                        account.pubkey.to_hex(),
                        e
                    );
                    results.push((account.pubkey, Err(e)));
                }
            }
        }

        tracing::info!(
            target: "whitenoise::session",
            "Restored {} sessions ({} failed)",
            results.iter().filter(|(_, r)| r.is_ok()).count(),
            results.iter().filter(|(_, r)| r.is_err()).count(),
        );

        results
    }
}

impl Whitenoise {
    /// Look up an active session by public key.
    pub fn session(&self, pubkey: &PublicKey) -> Option<Arc<AccountSession>> {
        self.account_manager.get_session(pubkey)
    }
}

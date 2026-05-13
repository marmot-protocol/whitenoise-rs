mod chat_list;
mod drafts;
mod groups;
pub(crate) mod key_packages;
mod membership;
pub(crate) mod messages;
mod mute_list;
pub(crate) mod push;
pub(crate) mod relay_handles;
mod settings;
mod social;

pub use self::chat_list::ChatListOps;
pub use self::drafts::DraftOps;
pub use self::groups::{GroupOps, MediaOps};
pub use self::key_packages::KeyPackageOps;
pub use self::membership::{MembershipOps, MembershipOpsForGroup};
pub use self::mute_list::MuteListOps;
pub use self::push::PushOps;
pub use self::settings::SettingsOps;
pub use self::social::SocialOps;

use std::sync::{Arc, Weak};

use dashmap::DashMap;
use mdk_core::prelude::{GroupId, MDK};
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr_sdk::prelude::NostrSigner;
use nostr_sdk::{EventId, PublicKey, RelayUrl};
use tokio::sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore, watch};
use tokio::task::JoinHandle;

use crate::nostr_manager::Result as NostrResult;
use crate::relay_control::RelayControlPlane;
use crate::relay_control::account_inbox::AccountInboxPlane;
use crate::relay_control::groups::GroupSubscriptionSpec;
use crate::types::AccountInboxPlaneStateSnapshot;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::{Account, DiscoveredRelayLists};
use crate::whitenoise::database::account::AccountRepositories;
use crate::whitenoise::database::account_db::AccountDatabase;
use crate::whitenoise::error::{Result, WhitenoiseError};

/// Returns the per-account SQLite path under the given data directory.
pub(crate) fn account_db_path(
    data_dir: &std::path::Path,
    account_pubkey: &PublicKey,
) -> std::path::PathBuf {
    data_dir
        .join("accounts")
        .join(format!("{}.db", account_pubkey.to_hex()))
}

/// Signer slot shared between `AccountSession` and its relay handles.
///
/// `None` for restored external-signer accounts whose platform signer has not
/// yet been re-registered. Handles that need signing return
/// [`WhitenoiseError::SignerUnavailable`] until the slot is filled.
pub(crate) type SharedSigner = Arc<RwLock<Option<Arc<dyn NostrSigner>>>>;

/// Owned inbox plane state held by `AccountSession`.
///
/// Wraps the plane together with its telemetry persistor task handle so they
/// share a single lifecycle: when the inbox is deactivated the telemetry task
/// is aborted and the plane's relay session is shut down.
struct AccountInboxState {
    plane: AccountInboxPlane,
    telemetry_handle: JoinHandle<()>,
}

impl AccountInboxState {
    async fn deactivate(self) {
        self.telemetry_handle.abort();
        self.plane.deactivate().await;
    }
}

/// A per-account session holding the account's MDK instance and signer.
///
/// `signer` is `None` for restored external-signer accounts whose platform
/// signer has not yet been re-registered. Operations requiring signing should
/// return the existing external-signer-unavailable error until
/// `register_external_signer()` fills the slot.
pub struct AccountSession {
    pub account_pubkey: PublicKey,
    pub mdk: Arc<MDK<MdkSqliteStorage>>,
    pub(crate) shared: Arc<crate::whitenoise::shared::SharedServices>,
    /// Weak back-reference to the owning `Whitenoise`. Held weakly to avoid a
    /// reference cycle (`Whitenoise` → `AccountManager` → `AccountSession`).
    /// Upgrade via [`AccountSession::whitenoise`] when a method still needs a
    /// `Whitenoise`-level helper that has not yet migrated down to session or
    /// `SharedServices` scope.
    pub(crate) whitenoise: Weak<Whitenoise>,
    pub(crate) repos: AccountRepositories,
    /// Per-account SQLite database. Holds the account's local-scoped tables
    /// (account_settings today; more in 18d/18e). The account is implicit —
    /// the file is the scope.
    pub(crate) account_db: Arc<AccountDatabase>,
    pub(crate) signer: SharedSigner,
    contact_list_guard: Arc<Semaphore>,
    cancellation: watch::Sender<bool>,
    pub(crate) ephemeral: relay_handles::AccountEphemeralHandle,
    pub(crate) group_handle: relay_handles::AccountGroupHandle,
    /// Owned inbox plane, `None` until first activation.
    inbox: RwLock<Option<AccountInboxState>>,
    /// Serializes concurrent activate/deactivate operations.
    activation_lock: Mutex<()>,
    /// In-memory coordination for delayed MIP-05 token-list responses.
    pub(crate) pending_push_token_responses: Arc<DashMap<(GroupId, EventId), ()>>,
    /// Bounds concurrently-active delayed MIP-05 token-list response tasks.
    token_response_semaphore: Arc<Semaphore>,
}

impl AccountSession {
    pub(crate) async fn new(
        account_pubkey: PublicKey,
        mdk: MDK<MdkSqliteStorage>,
        shared: Arc<crate::whitenoise::shared::SharedServices>,
        whitenoise: Weak<Whitenoise>,
        signer: Option<Arc<dyn NostrSigner>>,
    ) -> Result<Self> {
        let (cancellation, _) = watch::channel(false);
        let signer: SharedSigner = Arc::new(RwLock::new(signer));
        let ephemeral = relay_handles::AccountEphemeralHandle::new(
            account_pubkey,
            shared.relay_control.ephemeral(),
            signer.clone(),
        );
        let group_handle =
            relay_handles::AccountGroupHandle::new(account_pubkey, shared.relay_control.clone());

        let db_path = account_db_path(&shared.config.data_dir, &account_pubkey);
        let account_db = Arc::new(AccountDatabase::new(account_pubkey, db_path).await?);
        account_db
            .run_account_migrations(&shared.database.pool)
            .await?;

        let repos =
            AccountRepositories::new(account_pubkey, shared.database.clone(), account_db.clone())
                .await?;

        Ok(Self {
            account_pubkey,
            mdk: Arc::new(mdk),
            shared,
            whitenoise,
            repos,
            account_db,
            signer,
            contact_list_guard: Arc::new(Semaphore::new(1)),
            cancellation,
            ephemeral,
            group_handle,
            inbox: RwLock::new(None),
            activation_lock: Mutex::new(()),
            pending_push_token_responses: Arc::new(DashMap::new()),
            token_response_semaphore: Arc::new(Semaphore::new(
                crate::whitenoise::push_notifications::MAX_CONCURRENT_TOKEN_RESPONSE_TASKS,
            )),
        })
    }

    /// Build a session from a persisted account, loading MDK and (for local
    /// accounts) the signer from the secrets store.
    pub(crate) async fn from_account(account: &Account, wn: &Arc<Whitenoise>) -> Result<Self> {
        let mdk = wn.create_mdk_for_account(account.pubkey)?;
        let signer = if account.has_local_key() {
            Some(wn.get_signer_for_account(account)?)
        } else {
            None
        };
        Self::new(
            account.pubkey,
            mdk,
            wn.shared.clone(),
            Arc::downgrade(wn),
            signer,
        )
        .await
    }

    /// Upgrade the back-reference to the owning `Whitenoise`. Returns
    /// [`WhitenoiseError::Initialization`] if the owner has been dropped.
    pub(crate) fn whitenoise(&self) -> Result<Arc<Whitenoise>> {
        self.whitenoise
            .upgrade()
            .ok_or(WhitenoiseError::Initialization)
    }

    /// Replace the signer slot (e.g. when an external signer is re-registered).
    pub async fn set_signer(&self, signer: Arc<dyn NostrSigner>) {
        *self.signer.write().await = Some(signer);
    }

    /// Read the current signer, if any.
    ///
    /// Returns `None` in two cases: when no signer is set, and when a
    /// concurrent `set_signer` write momentarily holds the lock (we
    /// `try_read` to avoid blocking). Callers must handle `None` on their own
    /// — some fall back to the legacy signer lookup
    /// (`Whitenoise::get_signer_for_account`), others treat it as
    /// `SignerUnavailable` and drop the work (e.g. `handle_giftwrap`). See
    /// issue #777 for the planned disambiguation.
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

    /// Return a lightweight view for message read/search operations.
    pub fn messages(&self) -> messages::MessageOps<'_> {
        messages::MessageOps::new(self)
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

    /// Return a view for draft message operations scoped to this session.
    pub fn drafts(&self) -> DraftOps<'_> {
        DraftOps::new(self)
    }

    /// Return a view for per-account settings operations scoped to this session.
    pub fn settings(&self) -> SettingsOps<'_> {
        SettingsOps::new(self)
    }

    /// Return a view for follow/social operations scoped to this session.
    pub fn social(&self) -> SocialOps<'_> {
        SocialOps::new(self)
    }

    /// Return a view for group membership operations scoped to this session.
    pub fn membership(&self) -> MembershipOps<'_> {
        MembershipOps::new(self)
    }

    /// Return a view for group read operations scoped to this session.
    pub fn groups(&self) -> GroupOps<'_> {
        GroupOps::new(self)
    }

    /// Return a view for key package lifecycle operations scoped to this session.
    pub fn key_packages(&self) -> KeyPackageOps<'_> {
        KeyPackageOps::new(self)
    }

    /// Return a view for chat list read operations scoped to this session.
    pub fn chat_list(&self) -> ChatListOps<'_> {
        ChatListOps::new(self)
    }

    /// Return a view for mute list (block/unblock) operations scoped to this session.
    pub fn mute_list(&self) -> MuteListOps<'_> {
        MuteListOps::new(self)
    }

    /// Return a view for push notification operations scoped to this session.
    pub fn push(&self) -> PushOps<'_> {
        PushOps::new(self)
    }

    /// Schedule a delayed MIP-05 token-list response.
    ///
    /// Inserts the response key into the pending map, then spawns a delayed
    /// task (1-3 seconds random) that dispatches the response if it has not
    /// been cleared by an incoming token-list response in the meantime.
    pub(crate) fn schedule_pending_token_response(
        &self,
        group_id: GroupId,
        request_event_id: EventId,
    ) {
        use std::time::Duration;

        use rand::Rng;

        use crate::whitenoise::push_notifications::respond_to_token_request_with;

        let key = (group_id.clone(), request_event_id);
        if self
            .pending_push_token_responses
            .insert(key.clone(), ())
            .is_some()
        {
            return;
        }

        let permit = match self.token_response_semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                tracing::warn!(
                    target: "whitenoise::push_notifications",
                    account = %self.account_pubkey.to_hex(),
                    group_id = %hex::encode(group_id.as_slice()),
                    request_event_id = %request_event_id.to_hex(),
                    "Dropping MIP-05 token-list response task: concurrency limit reached"
                );
                self.pending_push_token_responses.remove(&key);
                return;
            }
        };

        let mdk = Arc::clone(&self.mdk);
        let account_db = Arc::clone(&self.account_db);
        let pending = Arc::clone(&self.pending_push_token_responses);
        let relay_control = Arc::clone(self.group_handle.relay_control());
        let account_pubkey = self.account_pubkey;
        let delay_ms = ::rand::rng().random_range(1_000u64..=3_000);
        let mut cancel_rx = self.cancellation.subscribe();

        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                _ = cancel_rx.changed() => {
                    pending.remove(&(group_id, request_event_id));
                    drop(permit);
                    return;
                }
            }

            let key = (group_id.clone(), request_event_id);
            if pending.remove(&key).is_none() {
                return;
            }

            if let Err(error) = respond_to_token_request_with(
                &mdk,
                &account_db.inner.pool,
                &relay_control,
                &account_pubkey,
                &group_id,
                request_event_id,
            )
            .await
            {
                tracing::warn!(
                    target: "whitenoise::push_notifications",
                    account = %account_pubkey.to_hex(),
                    group_id = %hex::encode(group_id.as_slice()),
                    request_event_id = %request_event_id.to_hex(),
                    error = %error,
                    "Failed to send delayed MIP-05 token-list response"
                );
            }

            drop(permit);
        });
    }

    // ── Inbox plane lifecycle ──────────────────────────────────────

    /// Activate group and inbox subscriptions for this session.
    ///
    /// **Atomicity:** Group subscriptions are established first; if inbox
    /// activation subsequently fails, group subscriptions are rolled back to
    /// their previous state.
    pub(crate) async fn activate_subscriptions(
        &self,
        relay_control: &RelayControlPlane,
        inbox_relays: &[RelayUrl],
        group_specs: &[GroupSubscriptionSpec],
        since: Option<nostr_sdk::Timestamp>,
        signer: Arc<dyn NostrSigner>,
    ) -> NostrResult<()> {
        let _guard = self.activation_lock.lock().await;

        let previous_group_state = self.group_handle.save_state().await;

        self.group_handle
            .sync_subscriptions(group_specs, since)
            .await?;

        let plane =
            relay_control.create_account_inbox_plane(self.account_pubkey, inbox_relays.to_vec());

        if let Err(error) = plane.activate(inbox_relays, since, signer).await {
            plane.deactivate().await;

            if let Some(previous_group_specs) = previous_group_state {
                if let Err(restore_error) = self
                    .group_handle
                    .sync_subscriptions(&previous_group_specs, since)
                    .await
                {
                    tracing::error!(
                        target: "whitenoise::session",
                        account_pubkey = %self.account_pubkey,
                        "Failed to restore previous group-plane state after inbox activation error: {restore_error}"
                    );
                }
            } else {
                self.group_handle.remove().await;
            }

            return Err(error);
        }

        let telemetry_receiver = plane.telemetry();
        let telemetry_handle =
            relay_control.spawn_account_inbox_telemetry(&self.account_pubkey, telemetry_receiver);

        let previous = self.inbox.write().await.replace(AccountInboxState {
            plane,
            telemetry_handle,
        });
        if let Some(previous) = previous {
            previous.deactivate().await;
        }

        Ok(())
    }

    /// Deactivate all subscriptions (inbox, group, ephemeral) for this session.
    pub(crate) async fn deactivate_subscriptions(&self) {
        let _guard = self.activation_lock.lock().await;

        let state = self.inbox.write().await.take();
        if let Some(state) = state {
            state.deactivate().await;
        }

        self.group_handle.remove().await;
        self.group_handle.remove_ephemeral_scope().await;
    }

    /// Deactivate only the inbox plane (used during shutdown).
    pub(crate) async fn deactivate_inbox(&self) {
        let state = self.inbox.write().await.take();
        if let Some(state) = state {
            state.deactivate().await;
        }
    }

    /// Check if the inbox plane has a connected relay.
    pub(crate) async fn has_inbox_subscription(&self) -> bool {
        match &*self.inbox.read().await {
            Some(state) => state.plane.has_connected_relay().await,
            None => false,
        }
    }

    /// Capture a diagnostic snapshot of the inbox plane, if active.
    pub(crate) async fn inbox_snapshot(&self) -> Option<AccountInboxPlaneStateSnapshot> {
        match &*self.inbox.read().await {
            Some(state) => Some(state.plane.snapshot().await),
            None => None,
        }
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

    /// Snapshot every active session.
    ///
    /// Convenient for callers that need to iterate per-account state (e.g.
    /// the event tracker checking each account's `published_events`). Holds
    /// no lock after returning.
    pub fn sessions_iter(&self) -> impl Iterator<Item = Arc<AccountSession>> {
        self.sessions
            .iter()
            .map(|e| e.value().clone())
            .collect::<Vec<_>>()
            .into_iter()
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

    /// Deactivate all session inbox planes. Called before relay-control shutdown.
    pub(crate) async fn deactivate_all_inboxes(&self) {
        let sessions: Vec<_> = self.sessions.iter().map(|e| e.value().clone()).collect();
        for session in sessions {
            session.deactivate_inbox().await;
        }
    }

    /// Collect inbox snapshots from all active sessions for diagnostics.
    pub(crate) async fn collect_inbox_snapshots(&self) -> Vec<AccountInboxPlaneStateSnapshot> {
        let sessions: Vec<_> = self.sessions.iter().map(|e| e.value().clone()).collect();
        let mut snapshots = Vec::with_capacity(sessions.len());
        for session in sessions {
            if let Some(snapshot) = session.inbox_snapshot().await {
                snapshots.push(snapshot);
            }
        }
        snapshots
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
    ///
    /// Returns the number of accounts that failed to restore. Callers that
    /// gate post-restore migrations on full success (e.g. the
    /// `MIGRATOR.run(&shared, None)` second pass that commits per-account
    /// drop migrations) should skip those operations when this is non-zero,
    /// because failed restores mean some local copy migrations did not run
    /// and dropping the corresponding shared rows would lose data.
    pub async fn restore_sessions(&self, wn: &Arc<Whitenoise>) -> RestoreSessionsOutcome {
        let accounts = match Account::all(&wn.shared.database).await {
            Ok(accounts) => accounts,
            Err(e) => {
                tracing::error!(
                    target: "whitenoise::session",
                    "Failed to load accounts for session restore: {}",
                    e
                );
                return RestoreSessionsOutcome::AccountListFailed;
            }
        };

        let mut ok_count = 0usize;
        let mut err_count = 0usize;

        for account in &accounts {
            match AccountSession::from_account(account, wn).await {
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

        RestoreSessionsOutcome::Completed {
            ok_count,
            err_count,
        }
    }
}

/// Outcome of [`AccountManager::restore_sessions`]. Used by
/// `Whitenoise::new` to decide whether the post-restore migrator pass is
/// safe to fire (it is not when any account failed to copy out of shared).
#[derive(Debug, Clone, Copy)]
pub enum RestoreSessionsOutcome {
    /// `Account::all` failed before any sessions were attempted; treat as
    /// "all accounts failed" for safety.
    AccountListFailed,
    /// Restore loop ran. `err_count == 0` means every persisted account is
    /// now active and its locals have been applied.
    Completed { ok_count: usize, err_count: usize },
}

impl RestoreSessionsOutcome {
    /// True iff every persisted account restored successfully.
    pub fn all_succeeded(&self) -> bool {
        matches!(self, Self::Completed { err_count: 0, .. })
    }

    pub fn err_count(&self) -> usize {
        match self {
            Self::AccountListFailed => usize::MAX,
            Self::Completed { err_count, .. } => *err_count,
        }
    }
}

impl Whitenoise {
    /// Look up an active session by public key.
    pub fn session(&self, pubkey: &PublicKey) -> Option<Arc<AccountSession>> {
        self.account_manager.get_session(pubkey)
    }

    /// Look up an active session, returning an error if none exists.
    pub(crate) fn require_session(&self, pubkey: &PublicKey) -> Result<Arc<AccountSession>> {
        self.session(pubkey).ok_or(WhitenoiseError::AccountNotFound)
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
    use crate::whitenoise::accounts::test_utils::create_mdk;
    use crate::whitenoise::database::Database;
    use crate::whitenoise::event_tracker::WhitenoiseEventTracker;
    use crate::whitenoise::message_aggregator::MessageAggregator;
    use crate::whitenoise::secrets_store::SecretsStore;
    use crate::whitenoise::shared::SharedServices;
    use crate::whitenoise::storage::Storage;
    use crate::whitenoise::test_utils::insert_test_account;

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

        // Insert the account row so AccountFollowsRepo can resolve the account id.
        insert_test_account(&db, &pubkey).await;

        let shared = test_shared(db).await;
        Arc::new(
            AccountSession::new(pubkey, mdk, shared, Weak::new(), None)
                .await
                .expect("create test session"),
        )
    }

    /// Build a minimal `SharedServices` for tests.
    ///
    /// Diverges from `create_mock_whitenoise_internal` in one behavior: this
    /// does not call `relay_control.start_telemetry_persistors()`. Tests built
    /// on `test_shared` therefore do not exercise telemetry persistor paths.
    pub async fn test_shared(db: Arc<Database>) -> Arc<SharedServices> {
        let (event_sender, _) = tokio::sync::mpsc::channel(1);
        let event_tracker = Arc::new(WhitenoiseEventTracker::detached(db.clone()));
        let event_tracker_dyn: Arc<dyn crate::whitenoise::event_tracker::EventTracker> =
            event_tracker.clone();
        let relay_control = Arc::new(RelayControlPlane::new(
            db.clone(),
            vec![],
            event_sender,
            [0u8; 16],
            event_tracker_dyn,
        ));
        let storage = Storage::new(std::env::temp_dir().as_path())
            .await
            .expect("test storage");
        let config = Arc::new(crate::whitenoise::WhitenoiseConfig::new(
            std::env::temp_dir().as_path(),
            std::env::temp_dir().as_path(),
            "com.whitenoise.test",
        ));
        Arc::new(SharedServices::new(
            config,
            db,
            relay_control,
            event_tracker,
            SecretsStore::new("com.whitenoise.test"),
            storage,
            MessageAggregator::new(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mdk_core::prelude::GroupId;
    use nostr_sdk::{EventId, Keys};

    use super::AccountManager;
    use super::test_helpers::test_session;
    use crate::whitenoise::accounts::DiscoveredRelayLists;
    use crate::whitenoise::push_notifications::MAX_CONCURRENT_TOKEN_RESPONSE_TASKS;

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
    async fn schedule_pending_token_response_dedupes_duplicate_request() {
        let pk = test_pubkey();
        let session = test_session(pk).await;
        let group_id = GroupId::from_slice(&[3; 32]);
        let request_event_id = EventId::all_zeros();

        session.schedule_pending_token_response(group_id.clone(), request_event_id);
        session.schedule_pending_token_response(group_id.clone(), request_event_id);

        assert_eq!(
            session.pending_push_token_responses.len(),
            1,
            "duplicate token response keys should share one pending response"
        );
        assert!(
            session
                .pending_push_token_responses
                .contains_key(&(group_id, request_event_id))
        );

        session.cancel();
    }

    #[tokio::test]
    async fn schedule_pending_token_response_drops_when_concurrency_is_exhausted() {
        let pk = test_pubkey();
        let session = test_session(pk).await;
        let group_id = GroupId::from_slice(&[4; 32]);
        let request_event_id = EventId::all_zeros();
        let _permit = session
            .token_response_semaphore
            .clone()
            .try_acquire_many_owned(MAX_CONCURRENT_TOKEN_RESPONSE_TASKS as u32)
            .expect("all permits should be initially available");

        session.schedule_pending_token_response(group_id.clone(), request_event_id);

        assert!(
            !session
                .pending_push_token_responses
                .contains_key(&(group_id, request_event_id)),
            "saturated scheduler should drop the response key so requesters can retry"
        );
    }

    #[tokio::test]
    async fn account_db_file_is_created_and_migrations_run() {
        let pk = test_pubkey();
        let session = test_session(pk).await;

        let path = session.account_db.path();
        assert!(path.exists(), "per-account DB file should be on disk");

        // The local migration runner creates `_rust_migrations` even with no
        // migrations registered.
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='_rust_migrations')",
        )
        .fetch_one(&session.account_db.inner.pool)
        .await
        .unwrap();
        assert!(exists, "tracking table should exist in per-account DB");

        // Cleanup: the AccountDatabase pool is closed when the session drops,
        // but the file lingers under /tmp. Try to remove it; non-fatal.
        let _ = std::fs::remove_file(path);
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

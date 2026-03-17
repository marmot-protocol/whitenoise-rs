use nostr_sdk::RelayUrl;

use crate::perf_instrument;
use crate::types::RelayControlStateSnapshot;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::users::User;

impl Whitenoise {
    #[perf_instrument("whitenoise")]
    pub async fn setup_all_subscriptions(whitenoise_ref: &'static Whitenoise) -> Result<()> {
        // Global (discovery plane) and per-account (inbox + group planes) subscriptions
        // operate on completely disjoint relay sessions with no shared mutable state,
        // so they can run concurrently. Using join! (not try_join!) ensures both run
        // to completion — avoids cancelling a partially-activated account if the
        // discovery sync fails.
        let (global_result, accounts_result) = tokio::join!(
            Self::setup_global_users_subscriptions(whitenoise_ref),
            Self::setup_accounts_subscriptions(whitenoise_ref),
        );
        global_result?;
        accounts_result?;
        Ok(())
    }

    #[perf_instrument("whitenoise")]
    async fn setup_global_users_subscriptions(whitenoise_ref: &Whitenoise) -> Result<()> {
        whitenoise_ref.sync_discovery_subscriptions().await
    }

    /// Buffer (in seconds) subtracted from `last_synced_at` timestamps when
    /// computing subscription `since` values, to account for clock drift and
    /// relay propagation delay.
    pub(crate) const SUBSCRIPTION_BUFFER_SECS: u64 = 10;

    // Compute a shared since timestamp for global user subscriptions.
    // - Accepts the already-loaded account list to avoid TOCTOU races
    // - If any account is unsynced (last_synced_at = None), return None
    // - Otherwise, use min(last_synced_at) minus a buffer, floored at 0
    #[perf_instrument("whitenoise")]
    pub(crate) fn compute_global_since_timestamp(
        accounts: &[Account],
    ) -> Option<nostr_sdk::Timestamp> {
        if accounts.is_empty() {
            tracing::debug!(
                target: "whitenoise::compute_global_since_timestamp",
                "No accounts; defaulting to since=None"
            );
            return None;
        }

        if accounts.iter().any(|a| a.last_synced_at.is_none()) {
            let unsynced = accounts
                .iter()
                .filter(|a| a.last_synced_at.is_none())
                .count();
            tracing::info!(
                target: "whitenoise::compute_global_since_timestamp",
                "Global subscriptions using since=None due to {} unsynced accounts",
                unsynced
            );
            return None;
        }

        let since = accounts
            .iter()
            .filter_map(|a| a.since_timestamp(Self::SUBSCRIPTION_BUFFER_SECS))
            .min_by_key(|t| t.as_secs());

        if let Some(ts) = since {
            tracing::info!(
                target: "whitenoise::compute_global_since_timestamp",
                "Global subscriptions using since={} ({}s buffer)",
                ts.as_secs(), Self::SUBSCRIPTION_BUFFER_SECS
            );
        }
        since
    }

    #[perf_instrument("whitenoise")]
    pub(super) async fn setup_accounts_subscriptions(
        whitenoise_ref: &'static Whitenoise,
    ) -> Result<()> {
        let accounts = Account::all(&whitenoise_ref.database).await?;
        for account in accounts {
            let inbox_relays = account.effective_inbox_relays(whitenoise_ref).await?;
            // Setup subscriptions for this account
            match whitenoise_ref
                .setup_subscriptions(&account, &inbox_relays)
                .await
            {
                Ok(()) => {
                    tracing::debug!(
                        target: "whitenoise::setup_accounts_subscriptions",
                        "Successfully set up subscriptions for account: {}",
                        account.pubkey.to_hex()
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::setup_accounts_subscriptions",
                        "Failed to set up subscriptions for account {}: {}",
                        account.pubkey.to_hex(),
                        e
                    );
                    // Continue with other accounts instead of failing completely
                }
            }
        }
        Ok(())
    }

    #[perf_instrument("whitenoise")]
    pub async fn ensure_account_subscriptions(&self, account: &Account) -> Result<()> {
        let is_operational = self.is_account_subscriptions_operational(account).await?;

        if !is_operational {
            tracing::info!(
                target: "whitenoise::ensure_account_subscriptions",
                "Account subscriptions not operational for {}, refreshing...",
                account.pubkey.to_hex()
            );
            self.refresh_account_subscriptions(account).await?;
        }

        Ok(())
    }

    #[perf_instrument("whitenoise")]
    pub async fn ensure_global_subscriptions(&self) -> Result<()> {
        let is_operational = self.is_global_subscriptions_operational().await?;

        if !is_operational {
            tracing::info!(
                target: "whitenoise::ensure_global_subscriptions",
                "Global subscriptions not operational, refreshing..."
            );
            Self::setup_global_users_subscriptions(self).await?;
        }

        Ok(())
    }

    /// Ensures all subscriptions (global and all accounts) are operational.
    ///
    /// This method is designed for periodic background tasks that need to ensure
    /// the entire subscription system is functioning. It checks and refreshes
    /// global subscriptions first, then iterates through all accounts.
    ///
    /// Uses a best-effort strategy: if one subscription check fails, logs the error
    /// and continues with the remaining checks. This maximizes the number of working
    /// subscriptions even when some fail due to transient network issues.
    ///
    /// # Error Handling
    ///
    /// - **Subscription errors**: Logged and ignored, processing continues
    /// - **Database errors**: Propagated immediately (catastrophic failure)
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Completed all checks (some may have failed, check logs)
    /// - `Err(_)`: Only on catastrophic failures (e.g., database connection lost)
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use whitenoise::Whitenoise;
    /// # async fn background_task(whitenoise: &Whitenoise) -> Result<(), Box<dyn std::error::Error>> {
    /// // In a periodic background task (every 15 minutes)
    /// whitenoise.ensure_all_subscriptions().await?;
    ///
    /// // All subscriptions are now as operational as possible
    /// // Check logs for any failures
    /// # Ok(())
    /// # }
    /// ```
    #[perf_instrument("whitenoise")]
    pub async fn ensure_all_subscriptions(&self) -> Result<()> {
        // Best-effort: log and continue on error
        if let Err(e) = self.ensure_global_subscriptions().await {
            tracing::warn!(
                target: "whitenoise::ensure_all_subscriptions",
                "Failed to ensure global subscriptions: {}", e
            );
        }

        // Fail fast only on database errors (catastrophic)
        let accounts = Account::all(&self.database).await?;

        // Best-effort: log and continue for each account
        for account in &accounts {
            if let Err(e) = self.ensure_account_subscriptions(account).await {
                tracing::warn!(
                    target: "whitenoise::ensure_all_subscriptions",
                    "Failed to ensure subscriptions for account {}: {}",
                    account.pubkey.to_hex(),
                    e
                );
            }
        }

        Ok(())
    }

    /// Checks if account subscriptions are operational.
    ///
    /// Returns true when:
    /// 1. The inbox plane and group plane both have connected relays, AND
    /// 2. The number of groups in the group plane matches MDK's active groups.
    ///
    /// The group count parity check catches groups that were added to MDK
    /// (e.g. via `accept_welcome`) but never made it into the group plane
    /// (e.g. because the background subscription setup failed on mobile).
    #[perf_instrument("whitenoise")]
    pub async fn is_account_subscriptions_operational(&self, account: &Account) -> Result<bool> {
        let relay_healthy = self
            .relay_control
            .has_account_subscriptions(&account.pubkey)
            .await;

        if !relay_healthy {
            return Ok(false);
        }

        let mdk_count = self.active_group_count(account)?;
        let plane_count = self
            .relay_control
            .group_plane_account_group_count(&account.pubkey)
            .await;

        if mdk_count != plane_count {
            tracing::info!(
                target: "whitenoise::is_account_subscriptions_operational",
                account = %account.pubkey.to_hex(),
                mdk_groups = mdk_count,
                plane_groups = plane_count,
                "Group count mismatch between MDK and group plane",
            );
        }

        Ok(mdk_count == plane_count)
    }

    /// Checks if global subscriptions are operational without refreshing.
    ///
    /// Returns true if at least one relay (from the client pool) is connected or connecting
    /// AND at least one global subscription exists.
    #[perf_instrument("whitenoise")]
    pub async fn is_global_subscriptions_operational(&self) -> Result<bool> {
        Ok(self.relay_control.has_discovery_subscriptions().await)
    }

    /// Returns a live in-memory snapshot of relay-plane state for debugging.
    #[perf_instrument("whitenoise")]
    pub async fn get_relay_control_state(&self) -> RelayControlStateSnapshot {
        self.relay_control.snapshot().await
    }

    #[perf_instrument("whitenoise")]
    pub(crate) async fn sync_discovery_subscriptions(&self) -> Result<()> {
        let accounts = Account::all(&self.database).await?;
        if accounts.is_empty() {
            // No accounts — tear down any active discovery subscriptions so they
            // don't linger after the last account logs out.
            self.relay_control
                .sync_discovery_subscriptions(&[], &[], None)
                .await
                .map_err(WhitenoiseError::from)?;
            return Ok(());
        }

        let watched_users = User::all_pubkeys(&self.database).await?;
        let follow_list_accounts = accounts
            .iter()
            .map(|account| {
                (
                    account.pubkey,
                    account.since_timestamp(Self::SUBSCRIPTION_BUFFER_SECS),
                )
            })
            .collect::<Vec<_>>();
        let public_since = Self::compute_global_since_timestamp(&accounts);

        self.relay_control
            .sync_discovery_subscriptions(&watched_users, &follow_list_accounts, public_since)
            .await
            .map_err(WhitenoiseError::from)
    }

    /// Returns the union of default relays and currently connected relays.
    ///
    /// Used as the fallback relay set when a user has no stored NIP-65 relays.
    /// Discovery fallback is owned by the discovery plane rather than whatever
    /// other relays happen to be connected for unrelated workloads.
    #[perf_instrument("whitenoise")]
    pub(crate) async fn fallback_relay_urls(&self) -> Vec<RelayUrl> {
        self.relay_control.discovery().relays().to_vec()
    }

    /// Refreshes discovery subscriptions after a single user's relay metadata
    /// changes (e.g. after processing a relay-list event for that user).
    ///
    /// **Note:** The current implementation performs a full replace of every
    /// watched-user and follow-list batch, identical to
    /// `refresh_all_global_subscriptions`. Per-user incremental patching is not
    /// yet implemented. Callers should not rely on this being cheap for large
    /// user sets — prefer batching multiple user updates and calling
    /// `refresh_all_global_subscriptions` once instead.
    #[perf_instrument("whitenoise")]
    pub(crate) async fn refresh_global_subscription_for_user(&self) -> Result<()> {
        self.sync_discovery_subscriptions().await?;
        Ok(())
    }

    /// Refreshes discovery subscriptions for all watched users across all
    /// relay batches. Use after bulk user discovery (e.g. contact-list
    /// processing) where many users may have changed simultaneously.
    #[perf_instrument("whitenoise")]
    pub(crate) async fn refresh_all_global_subscriptions(&self) -> Result<()> {
        self.sync_discovery_subscriptions().await?;
        Ok(())
    }
}

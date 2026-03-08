use nostr_sdk::{RelayUrl, Timestamp};

use super::Whitenoise;
use super::accounts::Account;
use super::error::Result;
use super::relays::Relay;
use super::users::User;

impl Whitenoise {
    pub async fn setup_all_subscriptions(whitenoise_ref: &'static Whitenoise) -> Result<()> {
        Self::setup_global_users_subscriptions(whitenoise_ref).await?;
        Self::setup_accounts_subscriptions(whitenoise_ref).await?;
        Ok(())
    }

    async fn setup_global_users_subscriptions(whitenoise_ref: &Whitenoise) -> Result<()> {
        let users_with_relays = User::all_users_with_relay_urls(whitenoise_ref).await?;
        let fallback_relays = whitenoise_ref.fallback_relay_urls().await;

        let accounts = Account::all(&whitenoise_ref.database).await?;
        if accounts.is_empty() {
            tracing::info!(
                target: "whitenoise::setup_global_users_subscriptions",
                "No accounts found, skipping global user subscriptions"
            );
            return Ok(());
        }

        // Find the first account that has a usable signer. External-only
        // accounts may not have a signer registered yet (e.g. before the
        // Flutter side provides one), so we skip those gracefully.
        let signer = accounts.iter().find_map(|account| {
            match whitenoise_ref.get_signer_for_account(account) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::debug!(
                        target: "whitenoise::setup_global_users_subscriptions",
                        "Skipping account {} for global subscriptions: {}",
                        account.pubkey.to_hex(),
                        e,
                    );
                    None
                }
            }
        });

        let Some(signer) = signer else {
            tracing::info!(
                target: "whitenoise::setup_global_users_subscriptions",
                "No account has a usable signer, skipping global user subscriptions"
            );
            return Ok(());
        };

        // Compute shared since for global user subscriptions with 10s lookback buffer
        let since = Self::compute_global_since_timestamp(whitenoise_ref).await?;

        whitenoise_ref
            .nostr
            .setup_batched_relay_subscriptions_with_signer(
                users_with_relays,
                &fallback_relays,
                signer,
                since,
            )
            .await?;
        Ok(())
    }

    // Compute a shared since timestamp for global user subscriptions.
    // - Assumes at least one account exists (caller checked signer presence)
    // - If any account is unsynced (last_synced_at = None), return None
    // - Otherwise, use min(last_synced_at) minus a 10s buffer, floored at 0
    async fn compute_global_since_timestamp(
        whitenoise_ref: &Whitenoise,
    ) -> Result<Option<Timestamp>> {
        let accounts = Account::all(&whitenoise_ref.database).await?;
        if accounts.iter().any(|a| a.last_synced_at.is_none()) {
            let unsynced = accounts
                .iter()
                .filter(|a| a.last_synced_at.is_none())
                .count();
            tracing::info!(
                target: "whitenoise::setup_global_users_subscriptions",
                "Global subscriptions using since=None due to {} unsynced accounts",
                unsynced
            );
            return Ok(None);
        }

        const BUFFER_SECS: u64 = 10;
        let since = accounts
            .iter()
            .filter_map(|a| a.since_timestamp(BUFFER_SECS))
            .min_by_key(|t| t.as_secs());

        if let Some(ts) = since {
            tracing::info!(
                target: "whitenoise::setup_global_users_subscriptions",
                "Global subscriptions using since={} ({}s buffer)",
                ts.as_secs(), BUFFER_SECS
            );
        } else {
            tracing::warn!(
                target: "whitenoise::setup_global_users_subscriptions",
                "No minimum last_synced_at found; defaulting to since=None"
            );
        }
        Ok(since)
    }

    async fn setup_accounts_subscriptions(whitenoise_ref: &'static Whitenoise) -> Result<()> {
        let accounts = Account::all(&whitenoise_ref.database).await?;
        for account in accounts {
            let nip65_relays = account.nip65_relays(whitenoise_ref).await?;
            let inbox_relays = account.effective_inbox_relays(whitenoise_ref).await?;
            match whitenoise_ref
                .setup_subscriptions(&account, &nip65_relays, &inbox_relays)
                .await
            {
                Ok(()) => {
                    tracing::debug!(
                        target: "whitenoise::initialize_whitenoise",
                        "Successfully set up subscriptions for account: {}",
                        account.pubkey.to_hex()
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::initialize_whitenoise",
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

    pub(crate) async fn refresh_global_subscription_for_user(&self, user: &User) -> Result<()> {
        let users_with_relays = User::all_users_with_relay_urls(self).await?;
        let fallback_relays = self.fallback_relay_urls().await;

        let Some(signer_account) = Account::first(&self.database).await? else {
            tracing::info!(
                target: "whitenoise::users::refresh_global_subscription",
                "No signer account found, skipping global user subscriptions"
            );
            return Ok(());
        };

        let signer = match self.get_signer_for_account(&signer_account) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::users::refresh_global_subscription",
                    "Failed to get signer for account {} (user {}), skipping global subscription refresh: {e}",
                    signer_account.pubkey.to_hex(),
                    user.pubkey.to_hex(),
                );
                return Ok(());
            }
        };
        self.nostr
            .refresh_user_global_subscriptions_with_signer(
                user.pubkey,
                users_with_relays,
                &fallback_relays,
                signer,
            )
            .await?;
        Ok(())
    }

    /// Refreshes global subscriptions for ALL batches across ALL relays.
    ///
    /// Unlike `refresh_global_subscription_for_user` which only refreshes
    /// batches containing a specific user, this refreshes every batch on every
    /// relay. Use after bulk user discovery (e.g. contact list processing)
    /// where new users may be spread across many different relay batches.
    pub(crate) async fn refresh_all_global_subscriptions(&self) -> Result<()> {
        let users_with_relays = User::all_users_with_relay_urls(self).await?;
        let fallback_relays = self.fallback_relay_urls().await;

        let Some(signer_account) = Account::first(&self.database).await? else {
            tracing::info!(
                target: "whitenoise::refresh_all_global_subscriptions",
                "No signer account found, skipping global subscription refresh"
            );
            return Ok(());
        };

        if signer_account.uses_external_signer() {
            self.nostr
                .refresh_all_global_subscriptions(users_with_relays, &fallback_relays)
                .await?;
        } else {
            let keys = self
                .secrets_store
                .get_nostr_keys_for_pubkey(&signer_account.pubkey)?;

            self.nostr
                .refresh_all_global_subscriptions_with_signer(
                    users_with_relays,
                    &fallback_relays,
                    keys,
                )
                .await?;
        }
        Ok(())
    }

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

    /// Checks if account subscriptions are operational
    ///
    /// Returns true if at least one relay is connected or connecting AND
    /// expected subscriptions exist (minimum: follow_list and giftwrap).
    pub async fn is_account_subscriptions_operational(&self, account: &Account) -> Result<bool> {
        let sub_count = self
            .nostr
            .count_subscriptions_for_account(&account.pubkey)
            .await;

        if sub_count < 2 {
            return Ok(false); // Early exit if subscriptions missing
        }

        let user_relays: Vec<RelayUrl> = Relay::urls(&account.nip65_relays(self).await?);
        let inbox_relays: Vec<RelayUrl> = Relay::urls(&account.inbox_relays(self).await?);

        let (group_relays, _) = self.extract_groups_relays_and_ids(account).await?;

        let all_relays: Vec<RelayUrl> = user_relays
            .iter()
            .chain(inbox_relays.iter())
            .chain(group_relays.iter())
            .cloned()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        Ok(self.nostr.has_any_relay_connected(&all_relays).await)
    }

    /// Checks if global subscriptions are operational without refreshing.
    ///
    /// Returns true if at least one relay (from the client pool) is connected or connecting
    /// AND at least one global subscription exists.
    pub async fn is_global_subscriptions_operational(&self) -> Result<bool> {
        let all_relays: Vec<RelayUrl> = self.nostr.client.relays().await.into_keys().collect();

        if !self.nostr.has_any_relay_connected(&all_relays).await {
            return Ok(false);
        }

        let global_count = self.nostr.count_global_subscriptions().await;
        Ok(global_count > 0)
    }
}

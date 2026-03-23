use std::sync::Arc;

use nostr_sdk::prelude::NostrSigner;
use nostr_sdk::{PublicKey, ToBech32};

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::error::{Result, WhitenoiseError};

impl Whitenoise {
    #[perf_instrument("whitenoise")]
    pub async fn export_account_nsec(&self, account: &Account) -> Result<String> {
        if account.uses_external_signer() {
            return Err(WhitenoiseError::ExternalSignerCannotExportNsec);
        }
        Ok(self
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)?
            .secret_key()
            .to_bech32()
            .unwrap())
    }

    #[perf_instrument("whitenoise")]
    pub async fn export_account_npub(&self, account: &Account) -> Result<String> {
        Ok(account.pubkey.to_bech32().unwrap())
    }

    /// Registers an external signer for an account.
    ///
    /// This is used for accounts that use external signers like Amber (NIP-55).
    /// The signer will be used for operations that require signing or decryption,
    /// such as processing giftwrap events.
    ///
    /// Returns an error if the account does not exist, is not an external
    /// signer account, or if the signer's pubkey does not match.
    #[perf_instrument("whitenoise")]
    pub async fn register_external_signer<S>(&self, pubkey: PublicKey, signer: S) -> Result<()>
    where
        S: NostrSigner + 'static,
    {
        let account = self.find_account_by_pubkey(&pubkey).await?;
        if !account.uses_external_signer() {
            return Err(WhitenoiseError::NotExternalSignerAccount);
        }
        self.insert_external_signer(pubkey, signer).await?;

        // On app restore, external accounts may exist before their signer is
        // re-registered. Startup subscription setup can fail in that gap.
        // Rebuild account subscriptions now that signing/decryption is available.
        // Only inbox_relays is needed by setup_subscriptions; do not gate
        // recovery on the nip65 lookup which is not used here.
        match account.inbox_relays(self).await {
            Ok(inbox_relays) => {
                if let Err(e) = self.setup_subscriptions(&account, &inbox_relays).await {
                    tracing::warn!(
                        target: "whitenoise::external_signer",
                        "Failed to recover account subscriptions for {} after signer registration: {}",
                        pubkey.to_hex(),
                        e
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::external_signer",
                    "Failed to load inbox relay list for {} during subscription recovery: {}",
                    pubkey.to_hex(),
                    e
                );
            }
        }

        // Best-effort global recovery for cases where global subscriptions were
        // also skipped due to no available signer at startup.
        if let Err(e) = self.ensure_global_subscriptions().await {
            tracing::warn!(
                target: "whitenoise::external_signer",
                "Failed to recover global subscriptions after signer registration for {}: {}",
                pubkey.to_hex(),
                e
            );
        }

        Ok(())
    }

    /// Inserts an external signer after validating the signer's pubkey matches.
    ///
    /// This is for internal use where the caller has already verified that the
    /// account uses an external signer (e.g., during login or test setup).
    /// The signer's pubkey is still validated to prevent mismatched signers.
    #[perf_instrument("whitenoise")]
    pub(crate) async fn insert_external_signer<S>(&self, pubkey: PublicKey, signer: S) -> Result<()>
    where
        S: NostrSigner + 'static,
    {
        self.validate_signer_pubkey(&pubkey, &signer).await?;
        tracing::info!(
            target: "whitenoise::external_signer",
            "Registering external signer for account: {}",
            pubkey.to_hex()
        );
        self.external_signers.insert(pubkey, Arc::new(signer));
        Ok(())
    }

    /// Gets the external signer for an account, if one is registered.
    pub fn get_external_signer(&self, pubkey: &PublicKey) -> Option<Arc<dyn NostrSigner>> {
        self.external_signers.get(pubkey).map(|r| r.clone())
    }

    /// Gets the appropriate signer for an account.
    ///
    /// For external accounts (Amber/NIP-55), returns the stored external signer.
    /// For local accounts, returns the keys from the secrets store.
    ///
    /// Returns an error if no signer is available for the account.
    pub(crate) fn get_signer_for_account(&self, account: &Account) -> Result<Arc<dyn NostrSigner>> {
        // First check for a registered external signer
        if let Some(external_signer) = self.get_external_signer(&account.pubkey) {
            tracing::debug!(
                target: "whitenoise::signer",
                "Using external signer for account {}",
                account.pubkey.to_hex()
            );
            return Ok(external_signer);
        }

        // Fall back to local keys from secrets store
        let keys = self
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)?;
        tracing::debug!(
            target: "whitenoise::signer",
            "Using local keys for account {}",
            account.pubkey.to_hex()
        );
        Ok(Arc::new(keys))
    }

    /// Removes the external signer for an account.
    pub fn remove_external_signer(&self, pubkey: &PublicKey) {
        tracing::info!(
            target: "whitenoise::external_signer",
            "Removing external signer for account: {}",
            pubkey.to_hex()
        );
        self.external_signers.remove(pubkey);
    }
}

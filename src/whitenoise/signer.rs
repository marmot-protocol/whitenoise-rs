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
        // Use effective_inbox_relays so legacy accounts that have no kind-10050
        // inbox list can still recover via the NIP-65 fallback.
        match account.effective_inbox_relays(self).await {
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
        if account.uses_external_signer() {
            let external_signer = self.get_external_signer(&account.pubkey).ok_or_else(|| {
                WhitenoiseError::Other(anyhow::anyhow!(
                    "External signer not registered for account {}",
                    account.pubkey.to_hex()
                ))
            })?;
            tracing::debug!(
                target: "whitenoise::signer",
                "Using external signer for account {}",
                account.pubkey.to_hex()
            );
            return Ok(external_signer);
        }

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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use nostr_sdk::Keys;

    use super::*;
    use crate::whitenoise::accounts::AccountType;
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    #[tokio::test]
    async fn test_get_signer_for_account_returns_error_for_external_account_without_registered_signer()
     {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let local_account = whitenoise.create_identity().await.unwrap();
        let external_account = Account {
            account_type: AccountType::External,
            ..local_account
        };

        let result = whitenoise.get_signer_for_account(&external_account);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_signer_for_account_ignores_external_signer_for_local_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let external_keys = Keys::generate();
        let external_pubkey = external_keys.public_key();

        whitenoise
            .insert_external_signer(external_pubkey, external_keys)
            .await
            .unwrap();

        let local_account = Account {
            id: None,
            pubkey: external_pubkey,
            user_id: 1,
            account_type: AccountType::Local,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let result = whitenoise.get_signer_for_account(&local_account);
        assert!(
            result.is_err(),
            "Local accounts must use local key path even when an external signer exists"
        );
    }

    #[tokio::test]
    async fn test_export_account_npub_returns_bech32_pubkey() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let exported = whitenoise.export_account_npub(&account).await.unwrap();
        assert_eq!(exported, account.pubkey.to_bech32().unwrap());
    }

    #[tokio::test]
    async fn test_export_account_nsec_rejects_external_signer_accounts() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let local_account = whitenoise.create_identity().await.unwrap();
        let external_account = Account {
            account_type: AccountType::External,
            ..local_account
        };

        let result = whitenoise.export_account_nsec(&external_account).await;
        assert!(matches!(
            result,
            Err(WhitenoiseError::ExternalSignerCannotExportNsec)
        ));
    }
}

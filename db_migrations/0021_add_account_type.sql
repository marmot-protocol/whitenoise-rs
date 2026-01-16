-- Add account_type column to support external signers (NIP-55)
-- account_type values:
--   'local' - Private key is stored locally in secrets store
--   'external' - Account uses external signer (e.g., Amber via NIP-55)

ALTER TABLE accounts ADD COLUMN account_type TEXT NOT NULL DEFAULT 'local';

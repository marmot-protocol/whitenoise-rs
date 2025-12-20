-- Create accounts_groups table to track account-group relationships
-- This is a join table between accounts and groups (via mls_group_id)
-- user_confirmation tracks whether the user has accepted or declined a group invite:
-- NULL = pending (auto-joined but awaiting user decision)
-- 1 (true) = accepted
-- 0 (false) = declined (hidden from UI)
CREATE TABLE accounts_groups (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_pubkey TEXT NOT NULL,
    mls_group_id BLOB NOT NULL,
    user_confirmation INTEGER DEFAULT NULL CHECK (user_confirmation IS NULL OR user_confirmation IN (0, 1)),
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE,
    UNIQUE(account_pubkey, mls_group_id)
);

-- Index for efficient lookups by account
CREATE INDEX idx_accounts_groups_account ON accounts_groups(account_pubkey);

-- Index for efficient lookups by group
CREATE INDEX idx_accounts_groups_group ON accounts_groups(mls_group_id);

-- Note: Existing groups are migrated lazily in the groups() method.
-- When groups() is called, any active MLS group without an AccountGroup record
-- will have one auto-created with user_confirmation = true (accepted).
-- This ensures existing users don't have to manually re-accept their groups.

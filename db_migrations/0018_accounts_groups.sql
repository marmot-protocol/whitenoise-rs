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
    user_confirmation INTEGER DEFAULT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE,
    UNIQUE(account_pubkey, mls_group_id)
);

-- Index for efficient lookups by account
CREATE INDEX idx_accounts_groups_account ON accounts_groups(account_pubkey);

-- Index for efficient lookups by group
CREATE INDEX idx_accounts_groups_group ON accounts_groups(mls_group_id);

-- Migrate existing groups from group_information as accepted (user_confirmation = 1)
-- These were created before auto-accept flow, so users explicitly accepted them
-- Using INSERT OR IGNORE to safely handle any potential duplicates
INSERT OR IGNORE INTO accounts_groups (account_pubkey, mls_group_id, user_confirmation, created_at, updated_at)
SELECT account_pubkey, mls_group_id, 1, created_at, updated_at
FROM group_information;

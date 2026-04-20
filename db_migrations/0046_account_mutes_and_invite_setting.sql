ALTER TABLE account_settings ADD COLUMN refuse_invites_from_muted_users INTEGER NOT NULL DEFAULT 1
    CHECK (refuse_invites_from_muted_users IN (0, 1));

CREATE TABLE account_muted_users (
    account_pubkey TEXT NOT NULL,
    muted_pubkey TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (account_pubkey, muted_pubkey),
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE
);

CREATE INDEX idx_account_muted_users_account ON account_muted_users (account_pubkey);

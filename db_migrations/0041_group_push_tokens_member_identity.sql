DROP INDEX IF EXISTS idx_group_push_tokens_account_group_leaf;
DROP INDEX IF EXISTS idx_group_push_tokens_account_group;
DROP INDEX IF EXISTS idx_group_push_tokens_account_server;

ALTER TABLE group_push_tokens RENAME TO group_push_tokens_old;

CREATE TABLE group_push_tokens (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_pubkey TEXT NOT NULL,
    mls_group_id BLOB NOT NULL,
    member_pubkey TEXT NOT NULL,
    leaf_index INTEGER NOT NULL CHECK (leaf_index >= 0),
    server_pubkey TEXT NOT NULL,
    relay_hint TEXT,
    encrypted_token TEXT NOT NULL CHECK (
        length(trim(encrypted_token, ' ' || char(9) || char(10) || char(13))) > 0
    ),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE
);

CREATE UNIQUE INDEX idx_group_push_tokens_account_group_leaf
    ON group_push_tokens(account_pubkey, mls_group_id, leaf_index);

CREATE INDEX idx_group_push_tokens_account_group
    ON group_push_tokens(account_pubkey, mls_group_id);

CREATE INDEX idx_group_push_tokens_account_group_member
    ON group_push_tokens(account_pubkey, mls_group_id, member_pubkey);

CREATE INDEX idx_group_push_tokens_account_server
    ON group_push_tokens(account_pubkey, server_pubkey);

DROP TABLE group_push_tokens_old;

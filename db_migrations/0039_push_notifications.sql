CREATE TABLE push_registrations (
    account_pubkey TEXT PRIMARY KEY,
    platform TEXT NOT NULL CHECK (platform IN ('apns', 'fcm')),
    raw_token TEXT NOT NULL CHECK (length(raw_token) > 0),
    server_pubkey TEXT NOT NULL,
    relay_hint TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    last_shared_at INTEGER,
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE
);

CREATE INDEX idx_push_registrations_server_pubkey
    ON push_registrations(server_pubkey);

CREATE TABLE group_push_tokens (
    account_pubkey TEXT NOT NULL,
    group_id BLOB NOT NULL,
    leaf_index INTEGER NOT NULL CHECK (leaf_index >= 0),
    server_pubkey TEXT NOT NULL,
    relay_hint TEXT,
    encrypted_token TEXT NOT NULL CHECK (length(encrypted_token) > 0),
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (account_pubkey, group_id, leaf_index),
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE
);

CREATE INDEX idx_group_push_tokens_account_group
    ON group_push_tokens(account_pubkey, group_id);

CREATE INDEX idx_group_push_tokens_account_server
    ON group_push_tokens(account_pubkey, server_pubkey);

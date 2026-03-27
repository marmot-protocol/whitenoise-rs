ALTER TABLE push_registrations RENAME TO push_registrations_old;
DROP INDEX idx_push_registrations_server_pubkey;

CREATE TABLE push_registrations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_pubkey TEXT NOT NULL,
    platform TEXT NOT NULL CHECK (platform IN ('apns', 'fcm')),
    raw_token TEXT NOT NULL CHECK (
        length(trim(raw_token, ' ' || char(9) || char(10) || char(13))) > 0
    ),
    server_pubkey TEXT NOT NULL,
    relay_hint TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    last_shared_at INTEGER,
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE
);

INSERT INTO push_registrations (
    account_pubkey,
    platform,
    raw_token,
    server_pubkey,
    relay_hint,
    created_at,
    updated_at,
    last_shared_at
)
SELECT
    account_pubkey,
    platform,
    raw_token,
    server_pubkey,
    relay_hint,
    created_at,
    updated_at,
    last_shared_at
FROM push_registrations_old;

CREATE UNIQUE INDEX idx_push_registrations_account_pubkey
    ON push_registrations(account_pubkey);

CREATE INDEX idx_push_registrations_server_pubkey
    ON push_registrations(server_pubkey);

DROP TABLE push_registrations_old;

ALTER TABLE group_push_tokens RENAME TO group_push_tokens_old;
DROP INDEX idx_group_push_tokens_account_group;
DROP INDEX idx_group_push_tokens_account_server;

CREATE TABLE group_push_tokens (
    account_pubkey TEXT NOT NULL,
    mls_group_id BLOB NOT NULL,
    leaf_index INTEGER NOT NULL CHECK (leaf_index >= 0),
    server_pubkey TEXT NOT NULL,
    relay_hint TEXT,
    encrypted_token TEXT NOT NULL CHECK (length(encrypted_token) > 0),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (account_pubkey, mls_group_id, leaf_index),
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE
);

INSERT INTO group_push_tokens (
    account_pubkey,
    mls_group_id,
    leaf_index,
    server_pubkey,
    relay_hint,
    encrypted_token,
    created_at,
    updated_at
)
SELECT
    account_pubkey,
    group_id,
    leaf_index,
    server_pubkey,
    relay_hint,
    encrypted_token,
    updated_at,
    updated_at
FROM group_push_tokens_old;

CREATE INDEX idx_group_push_tokens_account_group
    ON group_push_tokens(account_pubkey, mls_group_id);

CREATE INDEX idx_group_push_tokens_account_server
    ON group_push_tokens(account_pubkey, server_pubkey);

DROP TABLE group_push_tokens_old;

-- Account-database baseline schema.
--
-- Loaded by `m0012_bootstrap.rs` whenever a new account database is opened
-- for the first time. Captures the per-account schema at the bootstrap's
-- BASELINE_VERSION, so new accounts skip replaying historical local
-- data-migration logic that only matters for accounts upgraded from older
-- installs (mirror of `global/fresh_schema.sql` for the local timeline).
--
-- Rebaselining: when a future local migration does data extraction against
-- transient shared schema, consider rebaselining — bump
-- `BASELINE_VERSION` in `m0012_bootstrap.rs`, update this file to the
-- post-extraction schema, and add the now-skipped versions to
-- `STAMPED_PRIOR_LOCAL_VERSIONS`. New accounts then skip those locals
-- entirely instead of trying to replay them against shared state that may
-- have moved on.

CREATE TABLE IF NOT EXISTS account_settings (
    id                    INTEGER PRIMARY KEY CHECK (id = 1),
    notifications_enabled INTEGER NOT NULL DEFAULT 1,
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS drafts (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    mls_group_id       BLOB NOT NULL UNIQUE,
    content            TEXT NOT NULL DEFAULT '',
    reply_to_id        TEXT
        CHECK (reply_to_id IS NULL OR (length(reply_to_id) = 64 AND reply_to_id NOT GLOB '*[^0-9a-fA-F]*')),
    media_attachments  JSONB NOT NULL DEFAULT '[]',
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS published_key_packages (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    key_package_hash_ref  BLOB NOT NULL,
    event_id              TEXT NOT NULL UNIQUE,
    kind                  INTEGER NOT NULL DEFAULT 30443,
    d_tag                 TEXT NULL,
    consumed_at           INTEGER,
    key_material_deleted  INTEGER NOT NULL DEFAULT 0,
    created_at            INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_published_kp_event_id
    ON published_key_packages(event_id);

CREATE INDEX IF NOT EXISTS idx_published_kp_cleanup
    ON published_key_packages(consumed_at, key_material_deleted);

CREATE TABLE IF NOT EXISTS account_maintenance_tasks (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    name          TEXT NOT NULL UNIQUE,
    completed_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS published_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id    TEXT NOT NULL UNIQUE
        CHECK (length(event_id) = 64 AND event_id NOT GLOB '*[^0-9a-fA-F]*'),
    created_at  DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_published_events_event_id
    ON published_events(event_id);

CREATE TABLE IF NOT EXISTS processed_events (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id            TEXT NOT NULL UNIQUE
        CHECK (length(event_id) = 64 AND event_id NOT GLOB '*[^0-9a-fA-F]*'),
    created_at          DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    event_created_at    INTEGER DEFAULT NULL,
    event_kind          INTEGER DEFAULT NULL,
    author              TEXT DEFAULT NULL
);

CREATE INDEX IF NOT EXISTS idx_processed_events_kind_timestamp
    ON processed_events(event_kind, event_created_at);

CREATE TABLE IF NOT EXISTS account_follows (
    pubkey      TEXT NOT NULL PRIMARY KEY,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS media_references (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    mls_group_id        BLOB NOT NULL,
    encrypted_file_hash TEXT NOT NULL,
    media_type          TEXT NOT NULL,
    nostr_key           TEXT,
    file_metadata       BLOB,
    original_file_hash  TEXT,
    nonce               TEXT,
    scheme_version      TEXT,
    created_at          INTEGER NOT NULL,
    UNIQUE(mls_group_id, encrypted_file_hash)
);

CREATE INDEX IF NOT EXISTS idx_media_refs_group_hash
    ON media_references(mls_group_id, encrypted_file_hash);

CREATE INDEX IF NOT EXISTS idx_media_refs_hash
    ON media_references(encrypted_file_hash);

CREATE TABLE IF NOT EXISTS push_registrations (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    platform        TEXT NOT NULL CHECK (platform IN ('apns', 'fcm')),
    raw_token       TEXT NOT NULL CHECK (
        length(trim(raw_token, ' ' || char(9) || char(10) || char(13))) > 0
    ),
    server_pubkey   TEXT NOT NULL,
    relay_hint      TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    last_shared_at  INTEGER
);

CREATE INDEX IF NOT EXISTS idx_push_registrations_server_pubkey
    ON push_registrations(server_pubkey);

CREATE TABLE IF NOT EXISTS group_push_tokens (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    mls_group_id    BLOB NOT NULL,
    member_pubkey   TEXT NOT NULL,
    leaf_index      INTEGER NOT NULL CHECK (leaf_index >= 0),
    platform        TEXT CHECK (platform IN ('apns', 'fcm')),
    token_fingerprint TEXT CHECK (
        token_fingerprint IS NULL OR
        token_fingerprint GLOB 'sha256:[0-9a-f]*'
    ),
    server_pubkey   TEXT NOT NULL,
    relay_hint      TEXT,
    encrypted_token TEXT NOT NULL CHECK (
        length(trim(encrypted_token, ' ' || char(9) || char(10) || char(13))) > 0
    ),
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    UNIQUE(mls_group_id, leaf_index)
);

CREATE INDEX IF NOT EXISTS idx_group_push_tokens_group
    ON group_push_tokens(mls_group_id);

CREATE INDEX IF NOT EXISTS idx_group_push_tokens_group_member
    ON group_push_tokens(mls_group_id, member_pubkey);

CREATE INDEX IF NOT EXISTS idx_group_push_tokens_server
    ON group_push_tokens(server_pubkey);

CREATE TABLE IF NOT EXISTS accounts_groups (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    mls_group_id         BLOB NOT NULL UNIQUE,
    user_confirmation    INTEGER DEFAULT NULL
        CHECK (user_confirmation IS NULL OR user_confirmation IN (0, 1)),
    welcomer_pubkey      TEXT,
    last_read_message_id TEXT
        CHECK (last_read_message_id IS NULL OR
               (length(last_read_message_id) = 64
                AND last_read_message_id NOT GLOB '*[^0-9a-fA-F]*')),
    pin_order            INTEGER DEFAULT NULL,
    dm_peer_pubkey       TEXT DEFAULT NULL,
    archived_at          INTEGER DEFAULT NULL,
    removed_at           INTEGER DEFAULT NULL,
    self_removed         INTEGER NOT NULL DEFAULT 0,
    muted_until          INTEGER DEFAULT NULL,
    chat_cleared_at      INTEGER DEFAULT NULL,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_accounts_groups_group
    ON accounts_groups(mls_group_id);

CREATE INDEX IF NOT EXISTS idx_accounts_groups_dm_peer
    ON accounts_groups(dm_peer_pubkey)
    WHERE dm_peer_pubkey IS NOT NULL;

CREATE TABLE IF NOT EXISTS aggregated_messages (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id            TEXT NOT NULL
        CHECK (length(message_id) = 64 AND message_id GLOB '[0-9a-fA-F]*'),
    mls_group_id          BLOB NOT NULL,
    author                TEXT NOT NULL
        CHECK (length(author) = 64 AND author GLOB '[0-9a-fA-F]*'),
    created_at            INTEGER NOT NULL,
    kind                  INTEGER NOT NULL,
    content               TEXT NOT NULL DEFAULT '',
    tags                  JSONB NOT NULL,
    reply_to_id           TEXT
        CHECK (reply_to_id IS NULL OR
               (length(reply_to_id) = 64 AND reply_to_id GLOB '[0-9a-fA-F]*')),
    deletion_event_id     TEXT
        CHECK (deletion_event_id IS NULL OR
               (length(deletion_event_id) = 64 AND deletion_event_id GLOB '[0-9a-fA-F]*')),
    content_tokens        JSONB NOT NULL,
    reactions             JSONB NOT NULL,
    media_attachments     JSONB NOT NULL,
    content_normalized    TEXT NOT NULL DEFAULT '',
    UNIQUE(message_id, mls_group_id)
);

CREATE INDEX IF NOT EXISTS idx_aggregated_messages_message_id
    ON aggregated_messages(message_id);

CREATE INDEX IF NOT EXISTS idx_aggregated_messages_group
    ON aggregated_messages(mls_group_id, created_at);

CREATE INDEX IF NOT EXISTS idx_aggregated_messages_kind_group
    ON aggregated_messages(kind, mls_group_id, created_at DESC, message_id DESC);

CREATE TABLE IF NOT EXISTS marmot_message_projections (
    message_id   TEXT NOT NULL
        CHECK (length(message_id) = 64 AND message_id GLOB '[0-9a-fA-F]*'),
    mls_group_id BLOB NOT NULL,
    created_at   INTEGER NOT NULL,
    processed_at INTEGER NOT NULL,
    message_json TEXT NOT NULL,
    PRIMARY KEY (message_id, mls_group_id)
);

CREATE INDEX IF NOT EXISTS idx_marmot_message_projections_group
    ON marmot_message_projections(mls_group_id, created_at DESC, processed_at DESC);

CREATE TABLE IF NOT EXISTS message_delivery_status (
    message_id      TEXT NOT NULL,
    mls_group_id    BLOB NOT NULL,
    account_pubkey  TEXT NOT NULL,
    status          TEXT NOT NULL,
    PRIMARY KEY (message_id, mls_group_id, account_pubkey),
    FOREIGN KEY (message_id, mls_group_id)
        REFERENCES aggregated_messages(message_id, mls_group_id)
        ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_mds_group_account_status
    ON message_delivery_status (mls_group_id, account_pubkey, status);

-- Local mute_list (NIP-51 block list). Moved from shared in m0042; the
-- account_pubkey column is dropped because ownership is implicit in the file.
CREATE TABLE IF NOT EXISTS mute_list (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    muted_pubkey  TEXT NOT NULL UNIQUE
        CHECK (length(muted_pubkey) = 64
               AND muted_pubkey GLOB '[0-9a-fA-F]*'),
    is_private    INTEGER NOT NULL DEFAULT 1
        CHECK (is_private IN (0, 1)),
    created_at    INTEGER NOT NULL
);

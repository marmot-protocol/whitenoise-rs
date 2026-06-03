-- Consolidated base schema for fresh installs.
-- Generated from SQLx migrations 0001-0037 (last shipped in v2026.3.23).
-- Subsequent schema changes are applied by Rust migrations m0002+.

CREATE TABLE "accounts" (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pubkey TEXT NOT NULL UNIQUE,
    user_id INTEGER NOT NULL,
    last_synced_at DATETIME,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    account_type TEXT NOT NULL DEFAULT 'local'
        CHECK (account_type IN ('local', 'external'))
);

CREATE TABLE users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pubkey TEXT NOT NULL UNIQUE,
    metadata JSONB,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    metadata_known_at INTEGER NULL
);

CREATE TABLE account_follows (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL,
    user_id INTEGER NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX idx_account_follows_unique
    ON account_follows(account_id, user_id);

CREATE TABLE relays (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    url TEXT NOT NULL UNIQUE,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE user_relays (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL,
    relay_id INTEGER NOT NULL,
    relay_type TEXT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX idx_user_relays_unique
    ON user_relays (user_id, relay_id, relay_type);

CREATE TABLE app_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    theme_mode TEXT NOT NULL DEFAULT 'system',
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    language TEXT NOT NULL DEFAULT 'system'
);

INSERT OR IGNORE INTO app_settings (theme_mode) VALUES ('system');

CREATE TABLE group_information (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    mls_group_id BLOB NOT NULL UNIQUE,
    group_type TEXT NOT NULL DEFAULT 'group',
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE published_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL
        CHECK (length(event_id) = 64 AND event_id GLOB '[0-9a-fA-F]*'),
    account_id INTEGER NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE,
    UNIQUE(event_id, account_id)
);

CREATE INDEX idx_published_events_lookup ON published_events(event_id);
CREATE INDEX idx_published_events_account_id ON published_events(account_id);

CREATE TABLE processed_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL
        CHECK (length(event_id) = 64 AND event_id GLOB '[0-9a-fA-F]*'),
    account_id INTEGER,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    event_created_at INTEGER DEFAULT NULL,
    event_kind INTEGER DEFAULT NULL,
    author TEXT DEFAULT NULL,
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE,
    UNIQUE(event_id, account_id)
);

CREATE INDEX idx_processed_events_lookup ON processed_events(event_id);
CREATE INDEX idx_processed_events_account_id ON processed_events(account_id);
CREATE UNIQUE INDEX idx_processed_events_global_unique
    ON processed_events(event_id)
    WHERE account_id IS NULL;
CREATE INDEX idx_processed_events_event_account
    ON processed_events(event_id, account_id);
CREATE INDEX idx_processed_events_account_kind_timestamp
    ON processed_events(account_id, event_kind, event_created_at);
CREATE INDEX idx_processed_events_author_kind_timestamp
    ON processed_events(author, event_kind, event_created_at);
CREATE INDEX idx_processed_events_null_account_kind_timestamp
    ON processed_events(event_kind, event_created_at)
    WHERE account_id IS NULL;

CREATE TABLE media_files (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    mls_group_id BLOB NOT NULL,
    account_pubkey TEXT NOT NULL,
    file_path TEXT NOT NULL,
    encrypted_file_hash TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    media_type TEXT NOT NULL,
    blossom_url TEXT,
    nostr_key TEXT,
    file_metadata BLOB,
    created_at INTEGER NOT NULL,
    original_file_hash TEXT,
    nonce TEXT,
    scheme_version TEXT,
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE
);

CREATE INDEX idx_media_files_group_hash ON media_files(mls_group_id, encrypted_file_hash);
CREATE INDEX idx_media_files_account ON media_files(account_pubkey);
CREATE INDEX idx_media_files_created ON media_files(created_at);
CREATE INDEX idx_media_files_type ON media_files(media_type);
CREATE UNIQUE INDEX idx_media_files_unique
    ON media_files(mls_group_id, encrypted_file_hash, account_pubkey);
CREATE INDEX idx_media_files_scheme_version
    ON media_files(scheme_version)
    WHERE scheme_version IS NOT NULL;

CREATE TABLE aggregated_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id TEXT NOT NULL
        CHECK (length(message_id) = 64 AND message_id GLOB '[0-9a-fA-F]*'),
    mls_group_id BLOB NOT NULL,
    author TEXT NOT NULL
        CHECK (length(author) = 64 AND author GLOB '[0-9a-fA-F]*'),
    created_at INTEGER NOT NULL,
    kind INTEGER NOT NULL,
    content TEXT NOT NULL DEFAULT '',
    tags JSONB NOT NULL,
    reply_to_id TEXT
        CHECK (reply_to_id IS NULL OR (length(reply_to_id) = 64 AND reply_to_id GLOB '[0-9a-fA-F]*')),
    deletion_event_id TEXT
        CHECK (deletion_event_id IS NULL OR (length(deletion_event_id) = 64 AND deletion_event_id GLOB '[0-9a-fA-F]*')),
    content_tokens JSONB NOT NULL,
    reactions JSONB NOT NULL,
    media_attachments JSONB NOT NULL,
    content_normalized TEXT NOT NULL DEFAULT '',
    UNIQUE(message_id, mls_group_id),
    FOREIGN KEY (mls_group_id) REFERENCES group_information(mls_group_id) ON DELETE CASCADE
);

CREATE INDEX idx_aggregated_messages_message_id ON aggregated_messages(message_id);
CREATE INDEX idx_aggregated_messages_group ON aggregated_messages(mls_group_id, created_at);
CREATE INDEX idx_aggregated_messages_kind_group
    ON aggregated_messages(kind, mls_group_id, created_at);

CREATE TABLE accounts_groups (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_pubkey TEXT NOT NULL,
    mls_group_id BLOB NOT NULL,
    user_confirmation INTEGER DEFAULT NULL CHECK (user_confirmation IS NULL OR user_confirmation IN (0, 1)),
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    welcomer_pubkey TEXT,
    last_read_message_id TEXT
        CHECK (last_read_message_id IS NULL OR
               (length(last_read_message_id) = 64 AND last_read_message_id NOT GLOB '*[^0-9a-fA-F]*')),
    pin_order INTEGER DEFAULT NULL,
    dm_peer_pubkey TEXT DEFAULT NULL,
    archived_at INTEGER DEFAULT NULL,
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE,
    UNIQUE(account_pubkey, mls_group_id)
);

CREATE INDEX idx_accounts_groups_account ON accounts_groups(account_pubkey);
CREATE INDEX idx_accounts_groups_group ON accounts_groups(mls_group_id);
CREATE INDEX idx_accounts_groups_dm_peer
    ON accounts_groups(account_pubkey, dm_peer_pubkey)
    WHERE dm_peer_pubkey IS NOT NULL;

CREATE TABLE account_settings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_pubkey TEXT NOT NULL UNIQUE,
    notifications_enabled INTEGER NOT NULL DEFAULT 1 CHECK (notifications_enabled IN (0, 1)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE
);

CREATE TABLE drafts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_pubkey TEXT NOT NULL,
    mls_group_id BLOB NOT NULL,
    content TEXT NOT NULL DEFAULT '',
    reply_to_id TEXT
        CHECK (reply_to_id IS NULL OR (length(reply_to_id) = 64 AND reply_to_id NOT GLOB '*[^0-9a-fA-F]*')),
    media_attachments JSONB NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE,
    FOREIGN KEY (mls_group_id) REFERENCES group_information(mls_group_id) ON DELETE CASCADE,
    UNIQUE(account_pubkey, mls_group_id)
);

CREATE INDEX idx_drafts_group ON drafts(mls_group_id);

CREATE TABLE published_key_packages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_pubkey TEXT NOT NULL,
    key_package_hash_ref BLOB NOT NULL,
    event_id TEXT NOT NULL,
    consumed_at INTEGER,
    key_material_deleted INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE,
    UNIQUE(account_pubkey, event_id)
);

CREATE INDEX idx_published_kp_account ON published_key_packages(account_pubkey);
CREATE INDEX idx_published_kp_event_id ON published_key_packages(event_id);
CREATE INDEX idx_published_kp_cleanup
    ON published_key_packages(account_pubkey, consumed_at, key_material_deleted);

CREATE TABLE cached_graph_users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pubkey TEXT NOT NULL UNIQUE,
    metadata TEXT,
    follows TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    metadata_updated_at INTEGER,
    follows_updated_at INTEGER,
    metadata_expires_at INTEGER
);

CREATE INDEX idx_cached_graph_users_updated_at ON cached_graph_users(updated_at);
CREATE INDEX idx_cached_graph_users_metadata_updated_at ON cached_graph_users(metadata_updated_at);
CREATE INDEX idx_cached_graph_users_follows_updated_at ON cached_graph_users(follows_updated_at);
CREATE INDEX idx_cached_graph_users_metadata_expires_at ON cached_graph_users(metadata_expires_at);

CREATE TABLE message_delivery_status (
    message_id   TEXT NOT NULL,
    mls_group_id BLOB NOT NULL,
    status       TEXT NOT NULL,
    PRIMARY KEY (message_id, mls_group_id),
    FOREIGN KEY (message_id, mls_group_id)
        REFERENCES aggregated_messages(message_id, mls_group_id)
        ON DELETE CASCADE
);

CREATE INDEX idx_mds_group_status
    ON message_delivery_status (mls_group_id, status);

CREATE TABLE relay_status (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    relay_url TEXT NOT NULL,
    plane TEXT NOT NULL,
    account_pubkey TEXT
        CHECK (account_pubkey IS NULL OR (length(account_pubkey) = 64 AND account_pubkey NOT GLOB '*[^0-9a-fA-F]*')),
    last_connect_attempt_at INTEGER,
    last_connect_success_at INTEGER,
    last_failure_at INTEGER,
    failure_category TEXT,
    last_notice_reason TEXT,
    last_closed_reason TEXT,
    last_auth_reason TEXT,
    auth_required INTEGER NOT NULL DEFAULT 0 CHECK (auth_required IN (0, 1)),
    success_count INTEGER NOT NULL DEFAULT 0,
    failure_count INTEGER NOT NULL DEFAULT 0,
    latency_ms INTEGER,
    backoff_until INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX idx_relay_status_global_unique
    ON relay_status(relay_url, plane)
    WHERE account_pubkey IS NULL;
CREATE UNIQUE INDEX idx_relay_status_account_unique
    ON relay_status(relay_url, plane, account_pubkey)
    WHERE account_pubkey IS NOT NULL;
CREATE INDEX idx_relay_status_scope_updated
    ON relay_status (plane, account_pubkey, updated_at);

CREATE TABLE relay_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    relay_url TEXT NOT NULL,
    plane TEXT NOT NULL,
    account_pubkey TEXT
        CHECK (account_pubkey IS NULL OR (length(account_pubkey) = 64 AND account_pubkey NOT GLOB '*[^0-9a-fA-F]*')),
    occurred_at INTEGER NOT NULL,
    telemetry_kind TEXT NOT NULL,
    subscription_id TEXT,
    failure_category TEXT,
    message TEXT
);

CREATE INDEX idx_relay_events_scope_time
    ON relay_events (relay_url, plane, account_pubkey, occurred_at DESC);
CREATE INDEX idx_relay_events_kind_time
    ON relay_events (telemetry_kind, occurred_at DESC);

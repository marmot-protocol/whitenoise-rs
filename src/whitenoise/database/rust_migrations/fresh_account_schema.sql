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
    kind                  INTEGER NOT NULL DEFAULT 443,
    d_tag                 TEXT NULL,
    consumed_at           INTEGER,
    key_material_deleted  INTEGER NOT NULL DEFAULT 0,
    created_at            INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_published_kp_event_id
    ON published_key_packages(event_id);

CREATE INDEX IF NOT EXISTS idx_published_kp_cleanup
    ON published_key_packages(consumed_at, key_material_deleted);

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

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

-- Phase 1 relay observability storage.
--
-- `relay_status` stores the latest per-scope status snapshot keyed by
-- (relay_url, plane, account_pubkey). `account_pubkey` uses the empty string
-- when the scope is not account-specific so the uniqueness constraint works in
-- SQLite without NULL semantics creating duplicate "global" rows.
--
-- `relay_events` stores the append-only telemetry history. Retention/eviction
-- will be added in a later phase.

CREATE TABLE relay_status (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    relay_url TEXT NOT NULL,
    plane TEXT NOT NULL,
    account_pubkey TEXT NOT NULL DEFAULT ''
        CHECK (account_pubkey = '' OR (length(account_pubkey) = 64 AND account_pubkey NOT GLOB '*[^0-9a-fA-F]*')),
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
    updated_at INTEGER NOT NULL,
    UNIQUE(relay_url, plane, account_pubkey)
);

CREATE INDEX idx_relay_status_scope_updated
    ON relay_status (plane, account_pubkey, updated_at);

CREATE TABLE relay_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    relay_url TEXT NOT NULL,
    plane TEXT NOT NULL,
    account_pubkey TEXT NOT NULL DEFAULT ''
        CHECK (account_pubkey = '' OR (length(account_pubkey) = 64 AND account_pubkey NOT GLOB '*[^0-9a-fA-F]*')),
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

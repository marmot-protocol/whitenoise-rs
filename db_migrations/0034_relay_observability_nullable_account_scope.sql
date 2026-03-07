-- Align relay observability account scope handling with processed_events.
--
-- `account_pubkey` should be NULL when the telemetry/status scope is not tied
-- to a specific account. SQLite UNIQUE constraints do not treat NULL values as
-- equal, so relay_status uses partial unique indexes to preserve:
-- - one row per (relay_url, plane) when account_pubkey IS NULL
-- - one row per (relay_url, plane, account_pubkey) when account_pubkey IS NOT NULL

CREATE TABLE relay_status_new (
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

INSERT INTO relay_status_new (
    id,
    relay_url,
    plane,
    account_pubkey,
    last_connect_attempt_at,
    last_connect_success_at,
    last_failure_at,
    failure_category,
    last_notice_reason,
    last_closed_reason,
    last_auth_reason,
    auth_required,
    success_count,
    failure_count,
    latency_ms,
    backoff_until,
    created_at,
    updated_at
)
SELECT
    id,
    relay_url,
    plane,
    NULLIF(account_pubkey, ''),
    last_connect_attempt_at,
    last_connect_success_at,
    last_failure_at,
    failure_category,
    last_notice_reason,
    last_closed_reason,
    last_auth_reason,
    auth_required,
    success_count,
    failure_count,
    latency_ms,
    backoff_until,
    created_at,
    updated_at
FROM relay_status;

DROP TABLE relay_status;
ALTER TABLE relay_status_new RENAME TO relay_status;

CREATE UNIQUE INDEX idx_relay_status_global_unique
    ON relay_status(relay_url, plane)
    WHERE account_pubkey IS NULL;

CREATE UNIQUE INDEX idx_relay_status_account_unique
    ON relay_status(relay_url, plane, account_pubkey)
    WHERE account_pubkey IS NOT NULL;

CREATE INDEX idx_relay_status_scope_updated
    ON relay_status (plane, account_pubkey, updated_at);

CREATE TABLE relay_events_new (
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

INSERT INTO relay_events_new (
    id,
    relay_url,
    plane,
    account_pubkey,
    occurred_at,
    telemetry_kind,
    subscription_id,
    failure_category,
    message
)
SELECT
    id,
    relay_url,
    plane,
    NULLIF(account_pubkey, ''),
    occurred_at,
    telemetry_kind,
    subscription_id,
    failure_category,
    message
FROM relay_events;

DROP TABLE relay_events;
ALTER TABLE relay_events_new RENAME TO relay_events;

CREATE INDEX idx_relay_events_scope_time
    ON relay_events (relay_url, plane, account_pubkey, occurred_at DESC);

CREATE INDEX idx_relay_events_kind_time
    ON relay_events (telemetry_kind, occurred_at DESC);

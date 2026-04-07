-- NIP-51 kind 10000 mute list local cache.
-- Source of truth lives on Nostr relays; this table exists for fast
-- is_blocked() lookups in the message processing hot path.
CREATE TABLE mute_list (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    account_pubkey TEXT NOT NULL,
    muted_pubkey   TEXT NOT NULL,
    is_private     INTEGER NOT NULL DEFAULT 1 CHECK (is_private IN (0, 1)),
    created_at     INTEGER NOT NULL,
    UNIQUE(account_pubkey, muted_pubkey)
);

CREATE INDEX idx_mute_list_account ON mute_list(account_pubkey);

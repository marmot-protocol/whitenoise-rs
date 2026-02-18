-- Track the full lifecycle of every published KeyPackage from creation through cleanup.

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
CREATE INDEX idx_published_kp_cleanup ON published_key_packages(account_pubkey, consumed_at, key_material_deleted);

-- Track consumed KeyPackages for delayed local key material cleanup.
-- When a welcome is processed, the KeyPackage hash_ref is cached here
-- so we can delete local MLS key material after a quiet period,
-- even after the event has been deleted from relays.

CREATE TABLE consumed_key_packages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_pubkey TEXT NOT NULL,
    key_package_hash_ref BLOB NOT NULL,
    consumed_at INTEGER NOT NULL,
    UNIQUE(account_pubkey, key_package_hash_ref),
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE
);

CREATE INDEX idx_consumed_kp_account ON consumed_key_packages(account_pubkey);
CREATE INDEX idx_consumed_kp_timestamp ON consumed_key_packages(consumed_at);

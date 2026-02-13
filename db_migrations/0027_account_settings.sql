-- Per-account settings. Starts with notification preferences;
-- designed to grow as more user-configurable options are added.
CREATE TABLE account_settings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_pubkey TEXT NOT NULL UNIQUE,
    notifications_enabled INTEGER NOT NULL DEFAULT 1 CHECK (notifications_enabled IN (0, 1)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE
);

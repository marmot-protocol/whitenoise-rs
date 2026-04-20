-- Backfill account_muted_users for existing installs that already have mute_list data.
-- Idempotent: PRIMARY KEY(account_pubkey, muted_pubkey) + INSERT OR IGNORE prevents duplicates.
INSERT OR IGNORE INTO account_muted_users (
    account_pubkey,
    muted_pubkey,
    created_at,
    updated_at
)
SELECT
    account_pubkey,
    muted_pubkey,
    created_at,
    created_at
FROM mute_list;

-- Add dm_peer_pubkey column to accounts_groups for efficient DM peer lookups.
-- NULL for regular group chats. For DM groups, stores the other participant's
-- pubkey from this account's perspective. Populated at creation time and
-- backfilled on startup for existing records.
ALTER TABLE accounts_groups ADD COLUMN dm_peer_pubkey TEXT DEFAULT NULL;

-- Partial index for efficient DM peer lookups. Only indexes rows where
-- dm_peer_pubkey is populated (DM groups), keeping the index small.
CREATE INDEX idx_accounts_groups_dm_peer
  ON accounts_groups(account_pubkey, dm_peer_pubkey)
  WHERE dm_peer_pubkey IS NOT NULL;

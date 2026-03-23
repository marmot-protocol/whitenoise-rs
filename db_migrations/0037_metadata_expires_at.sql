-- Per-entry cache expiration for metadata.
--
-- Replaces the single 24-hour TTL applied uniformly at query time.
-- Callers now set expiration at write time based on confidence:
--   - Confident miss (clean EOSE): 24 hours
--   - Uncertain miss (relay errors): 30 minutes
--
-- Backfill: existing rows with metadata get expires_at = metadata_updated_at + 24h.
-- NULL means "expired" (safe default for pre-migration rows without metadata).

ALTER TABLE cached_graph_users ADD COLUMN metadata_expires_at INTEGER;

UPDATE cached_graph_users
SET metadata_expires_at = metadata_updated_at + 86400000
WHERE metadata_updated_at IS NOT NULL;

CREATE INDEX idx_cached_graph_users_metadata_expires_at ON cached_graph_users(metadata_expires_at);

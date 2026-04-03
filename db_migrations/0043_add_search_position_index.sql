-- Extend the primary message index to include message_id as a tiebreaker
-- for deterministic ordering when multiple messages share the same created_at.
--
-- The search query computes position via:
--   ROW_NUMBER() OVER (ORDER BY created_at DESC, message_id DESC)
-- and the existing idx_aggregated_messages_kind_group covers (kind, mls_group_id, created_at)
-- but not message_id, forcing SQLite to sort on the tiebreaker column.
--
-- This index lets the window function and ORDER BY scan in index order without
-- a temp B-tree sort. deletion_event_id is included for the IS NULL filter.
--
-- Tradeoff: slightly larger index footprint in exchange for O(matched) sort cost
-- instead of O(group_size) for the ROW_NUMBER window over the full group.
CREATE INDEX idx_aggregated_messages_kind_group_position
    ON aggregated_messages(kind, mls_group_id, deletion_event_id, created_at DESC, message_id DESC);

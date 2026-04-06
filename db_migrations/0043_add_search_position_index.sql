-- Replace the existing (kind, mls_group_id, created_at) index with one that
-- also includes message_id DESC as a tiebreaker for deterministic ordering.
--
-- The search query computes position via:
--   ROW_NUMBER() OVER (ORDER BY created_at DESC, message_id DESC)
--
-- Without message_id in the index, SQLite must use a temp B-tree sort for the
-- tiebreaker column. This index lets the window function and ORDER BY scan in
-- index order without a temp B-tree sort.
DROP INDEX IF EXISTS idx_aggregated_messages_kind_group;

CREATE INDEX idx_aggregated_messages_kind_group
    ON aggregated_messages(kind, mls_group_id, created_at DESC, message_id DESC);

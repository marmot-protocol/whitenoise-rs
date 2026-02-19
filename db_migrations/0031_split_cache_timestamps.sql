-- Add metadata_updated_at and follows_updated_at alongside existing updated_at.
-- Each field now tracks its own freshness independently, preventing
-- follows refreshes from keeping stale empty metadata appearing "fresh".
-- updated_at is retained as a general "any write" timestamp for cleanup.

CREATE TABLE cached_graph_users_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pubkey TEXT NOT NULL UNIQUE,
    metadata TEXT,
    follows TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    metadata_updated_at INTEGER,
    follows_updated_at INTEGER
);

-- Carry forward updated_at into whichever field has data.
-- If metadata is non-NULL, it was fetched → set metadata_updated_at.
-- If follows is non-NULL, it was fetched → set follows_updated_at.
INSERT INTO cached_graph_users_new (id, pubkey, metadata, follows, created_at, updated_at, metadata_updated_at, follows_updated_at)
SELECT id, pubkey, metadata, follows, created_at, updated_at,
    CASE WHEN metadata IS NOT NULL THEN updated_at ELSE NULL END,
    CASE WHEN follows IS NOT NULL THEN updated_at ELSE NULL END
FROM cached_graph_users;

DROP TABLE cached_graph_users;
ALTER TABLE cached_graph_users_new RENAME TO cached_graph_users;
CREATE INDEX idx_cached_graph_users_updated_at ON cached_graph_users(updated_at);
CREATE INDEX idx_cached_graph_users_metadata_updated_at ON cached_graph_users(metadata_updated_at);
CREATE INDEX idx_cached_graph_users_follows_updated_at ON cached_graph_users(follows_updated_at);

-- Make metadata and follows nullable in cached_graph_users.
-- NULL = not yet fetched, '{}'/`'[]'` = fetched but empty, non-empty = real data.
-- This allows split fetching: metadata-only writes won't clobber follows (and vice versa).

CREATE TABLE cached_graph_users_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pubkey TEXT NOT NULL UNIQUE,
    metadata TEXT,
    follows TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

INSERT INTO cached_graph_users_new (id, pubkey, metadata, follows, created_at, updated_at)
SELECT id, pubkey, metadata, follows, created_at, updated_at FROM cached_graph_users;

DROP TABLE cached_graph_users;
ALTER TABLE cached_graph_users_new RENAME TO cached_graph_users;
CREATE INDEX idx_cached_graph_users_updated_at ON cached_graph_users(updated_at);

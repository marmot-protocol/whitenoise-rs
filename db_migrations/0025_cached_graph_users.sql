-- Cached user data for users discovered via social graph traversal (web of trust).
-- These are cached BEFORE any search filtering - they represent the "search space".
-- Stores both metadata and follows to avoid creating User records.
-- Has 24-hour TTL for freshness.
CREATE TABLE cached_graph_users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pubkey TEXT NOT NULL UNIQUE,
    metadata TEXT NOT NULL,
    follows TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX idx_cached_graph_users_updated_at ON cached_graph_users(updated_at);

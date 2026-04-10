-- Migration 0044: Add expires_at column to aggregated_messages
--
-- Supports disappearing messages: when a group has a disappearing message
-- duration configured, new messages are stored with an expires_at timestamp.
-- A scheduled cleanup task periodically deletes rows past their expiry.
--
-- NULL means the message does not expire. Only non-NULL rows are indexed
-- so the cleanup query stays efficient regardless of how many permanent
-- messages exist.

ALTER TABLE aggregated_messages ADD COLUMN expires_at INTEGER;

CREATE INDEX idx_aggregated_messages_expires_at
    ON aggregated_messages(expires_at) WHERE expires_at IS NOT NULL;

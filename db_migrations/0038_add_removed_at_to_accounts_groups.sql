-- Add removed_at column to accounts_groups to track when a user was
-- involuntarily removed from a group by an admin.
-- NULL = not removed (default). When set, stores the millisecond
-- timestamp of when the removal commit was processed.
-- Removed groups remain visible in the chat list (read-only, with a
-- system message) until the user explicitly archives or deletes them.
ALTER TABLE accounts_groups ADD COLUMN removed_at INTEGER DEFAULT NULL;

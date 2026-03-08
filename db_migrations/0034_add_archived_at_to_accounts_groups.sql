-- Add archived_at column to accounts_groups for chat archiving.
-- NULL = not archived (default). When set, stores the millisecond
-- timestamp of when the chat was archived.
ALTER TABLE accounts_groups ADD COLUMN archived_at INTEGER DEFAULT NULL;

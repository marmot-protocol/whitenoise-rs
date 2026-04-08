-- Add chat_cleared_at column for the "clear chat" feature.
-- chat_cleared_at: messages at or before this timestamp are hidden from the UI.
ALTER TABLE accounts_groups ADD COLUMN chat_cleared_at INTEGER DEFAULT NULL;

-- Add pin_order column to accounts_groups for chat pinning functionality.
-- NULL = not pinned (default), lower values appear first in the chat list.
ALTER TABLE accounts_groups ADD COLUMN pin_order INTEGER DEFAULT NULL;

-- Add muted_until column to accounts_groups for per-chat mute.
-- NULL = not muted (default). When set, stores the millisecond
-- timestamp of when the mute expires. A far-future value represents
-- "muted forever" (until manually unmuted).
ALTER TABLE accounts_groups ADD COLUMN muted_until INTEGER DEFAULT NULL;

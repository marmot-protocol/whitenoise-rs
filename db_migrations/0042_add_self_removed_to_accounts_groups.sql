ALTER TABLE accounts_groups ADD COLUMN self_removed INTEGER NOT NULL DEFAULT 0 CHECK (self_removed IN (0, 1));

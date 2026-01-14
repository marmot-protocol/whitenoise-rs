-- Add last_read_message_id to track read marker per account-group
ALTER TABLE accounts_groups ADD COLUMN last_read_message_id TEXT
    CHECK (last_read_message_id IS NULL OR
           (length(last_read_message_id) = 64 AND last_read_message_id NOT GLOB '*[^0-9a-fA-F]*'));

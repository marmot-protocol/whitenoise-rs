-- Migration 0019: Add welcomer_pubkey to accounts_groups table
-- This column will store the public key that is inviting the user to the group.
ALTER TABLE accounts_groups ADD COLUMN welcomer_pubkey TEXT;

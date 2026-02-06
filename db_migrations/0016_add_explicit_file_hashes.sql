-- Migration 0016: Add explicit file hash naming for MIP-04 compliance
--
-- This migration renames the ambiguous 'file_hash' column and adds explicit support
-- for both the original content hash and encrypted blob hash.
--
-- Both hashes serve distinct purposes:
-- - original_file_hash: SHA-256 of decrypted content (for MIP-04 x field, MDK decryption)
-- - encrypted_file_hash: SHA-256 of encrypted blob (for Blossom verification)
--
-- MIP-04 specification requires the imeta tag's 'x' field to contain the original
-- content hash, not the encrypted hash. This migration ensures we can store and use
-- both hashes correctly.
--
-- See: MIP-04 specification https://github.com/parres-hq/marmot/blob/master/04.md
-- See: MDK decrypt_from_download() requires original_hash for key derivation

-- Step 1: Rename file_hash to encrypted_file_hash for explicit naming
-- SQLite's RENAME COLUMN automatically updates all indexes, triggers, and constraints
ALTER TABLE media_files RENAME COLUMN file_hash TO encrypted_file_hash;

-- Step 2: Add original_file_hash column
-- NULL for existing records only (cannot be backfilled without re-downloading)
--
-- Note: encrypted_file_hash remains NOT NULL because:
-- - For uploaded files: we compute it during encryption
-- - For received files: we extract it from the Blossom URL (required by MIP-04)
-- - Malformed imeta tags (unparseable URLs) are skipped with warnings, not stored
ALTER TABLE media_files ADD COLUMN original_file_hash TEXT;

-- Note: Existing indexes are automatically updated by the RENAME COLUMN:
-- - idx_media_files_group_hash: now uses (mls_group_id, encrypted_file_hash)
-- - idx_media_files_unique: now uses (mls_group_id, encrypted_file_hash, account_pubkey)

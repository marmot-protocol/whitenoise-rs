-- Migration 0021: Add encryption metadata to media_files for MIP-04 decryption
--
-- This migration adds nonce and scheme_version columns required for MDK's
-- decrypt_from_download() function. These values are extracted from the IMETA tag
-- when receiving chat media.
--
-- Both fields are required for decryption:
-- - nonce: Encryption nonce (hex-encoded, 24 chars for 12 bytes)
-- - scheme_version: Encryption version (e.g., "mip04-v2")
--
-- IMPORTANT: Old chat_media records (created before this migration) cannot be
-- decrypted because they lack these required fields. This is a breaking change
-- from MDK - the encryption parameters were previously not stored in IMETA tags.
-- Group images are unaffected as they use a different encryption scheme (key/nonce).
--
-- See: MDK MediaReference struct requirements
-- See: MIP-04 specification https://github.com/marmot-protocol/marmot/blob/master/04.md

-- Add nonce column
-- NULL for existing records and group_images (which use key/nonce encryption, not MDK)
-- Required for chat_media going forward
ALTER TABLE media_files ADD COLUMN nonce TEXT;

-- Add scheme_version column
-- NULL for existing records and group_images
-- Required for chat_media going forward
ALTER TABLE media_files ADD COLUMN scheme_version TEXT;

-- Add index for scheme_version to optimize queries filtering by encryption version
-- Partial index (WHERE NOT NULL) for better performance - only indexes chat_media records
CREATE INDEX idx_media_files_scheme_version
ON media_files(scheme_version)
WHERE scheme_version IS NOT NULL;

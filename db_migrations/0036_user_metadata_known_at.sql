-- Track whether user metadata has been resolved yet.
-- `NULL` = unknown / not yet resolved.
-- non-NULL = legacy metadata resolution completed and backfilled from `updated_at`,
-- excluding empty JSON `{}` objects.
--
-- `nostr_sdk::Metadata::new()` serializes to `{}` in the current dependency set,
-- and `json_each(users.metadata)` returns no rows for that shape, so legacy rows
-- with exactly that empty object stay unknown. Any row with a non-empty JSON
-- object is backfilled as known using its existing `updated_at`.

ALTER TABLE users
ADD COLUMN metadata_known_at INTEGER NULL;

UPDATE users
SET metadata_known_at = CASE
    WHEN metadata IS NOT NULL
        AND json_valid(metadata) = 1
        AND json_type(metadata) = 'object'
        AND EXISTS (SELECT 1 FROM json_each(users.metadata))
    THEN updated_at
    ELSE NULL
END;

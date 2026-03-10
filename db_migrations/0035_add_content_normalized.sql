-- Add content_normalized column for Unicode-aware case-insensitive full-text search.
--
-- Stores NFC-normalized, Unicode-lowercased content so that SQLite LIKE comparisons
-- work correctly across all scripts, including those where SQLite's built-in LOWER()
-- is a no-op (Cyrillic, Greek, Turkish, Armenian, etc.).
--
-- The column is populated at insert/update time by the application layer.
-- Existing rows get an empty string as a safe default; they will be re-matched
-- only after the next re-sync (acceptable: search is best-effort on cached data).
ALTER TABLE aggregated_messages ADD COLUMN content_normalized TEXT NOT NULL DEFAULT '';

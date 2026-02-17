-- Unsent message drafts, one per (account, group).
-- Stores work-in-progress text, an optional reply-to reference,
-- and any already-uploaded media attachments as JSONB.
CREATE TABLE drafts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_pubkey TEXT NOT NULL,
    mls_group_id BLOB NOT NULL,
    content TEXT NOT NULL DEFAULT '',
    reply_to_id TEXT
        CHECK (reply_to_id IS NULL OR (length(reply_to_id) = 64 AND reply_to_id NOT GLOB '*[^0-9a-fA-F]*')),
    media_attachments JSONB NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL,  -- Unix timestamp in milliseconds
    updated_at INTEGER NOT NULL,  -- Unix timestamp in milliseconds
    FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE,
    FOREIGN KEY (mls_group_id) REFERENCES group_information(mls_group_id) ON DELETE CASCADE,
    UNIQUE(account_pubkey, mls_group_id)
);

CREATE INDEX idx_drafts_group ON drafts(mls_group_id);

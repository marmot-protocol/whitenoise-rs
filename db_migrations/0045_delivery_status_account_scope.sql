-- Fix delivery-status account scope bug (GitHub issue #739).
--
-- The message_delivery_status table was keyed by (message_id, mls_group_id)
-- with no account_pubkey, so sender-local delivery state bled across accounts
-- in the same group.  This migration adds account_pubkey to the primary key.
--
-- Backfill strategy: join on aggregated_messages.author to attribute existing
-- rows to the message sender.  Rows that cannot be attributed (author NULL or
-- missing) are dropped — they represent stale state that will self-correct on
-- next message send.

-- SQLite cannot ALTER a PRIMARY KEY, so we recreate the table.

CREATE TABLE message_delivery_status_new (
    message_id      TEXT NOT NULL,
    mls_group_id    BLOB NOT NULL,
    account_pubkey  TEXT NOT NULL,
    status          TEXT NOT NULL,
    PRIMARY KEY (message_id, mls_group_id, account_pubkey),
    FOREIGN KEY (message_id, mls_group_id)
        REFERENCES aggregated_messages(message_id, mls_group_id)
        ON DELETE CASCADE
);

-- Backfill: attribute existing rows to the message author.
INSERT INTO message_delivery_status_new (message_id, mls_group_id, account_pubkey, status)
SELECT
    mds.message_id,
    mds.mls_group_id,
    am.author,
    mds.status
FROM message_delivery_status mds
INNER JOIN aggregated_messages am
    ON am.message_id = mds.message_id
   AND am.mls_group_id = mds.mls_group_id
WHERE am.author IS NOT NULL;

DROP TABLE message_delivery_status;
ALTER TABLE message_delivery_status_new RENAME TO message_delivery_status;

-- Recreate index with account_pubkey for per-account queries.
CREATE INDEX idx_mds_group_account_status
    ON message_delivery_status (mls_group_id, account_pubkey, status);

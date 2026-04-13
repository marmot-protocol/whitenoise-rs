-- Add account_pubkey to message_delivery_status primary key.
--
-- The old table keyed by (message_id, mls_group_id) allows delivery state to
-- collide when two local accounts belong to the same group.  Adding
-- account_pubkey scopes the status per-account.

-- 1. Create the new table with the expanded primary key.
CREATE TABLE message_delivery_status_v2 (
    account_pubkey TEXT    NOT NULL,
    message_id     TEXT    NOT NULL,
    mls_group_id   BLOB   NOT NULL,
    status         TEXT    NOT NULL,
    PRIMARY KEY (account_pubkey, message_id, mls_group_id),
    FOREIGN KEY (message_id, mls_group_id)
        REFERENCES aggregated_messages(message_id, mls_group_id)
        ON DELETE CASCADE
);

-- 2. Copy existing rows, filling account_pubkey from the message author.
--    Delivery status is only created for outgoing messages, so the author in
--    aggregated_messages is the local account that sent it.  The INNER JOIN on
--    accounts ensures we only copy rows whose author is actually a local account
--    (filtering out any hypothetical stale rows whose author was removed).
INSERT INTO message_delivery_status_v2 (account_pubkey, message_id, mls_group_id, status)
SELECT am.author, mds.message_id, mds.mls_group_id, mds.status
FROM message_delivery_status mds
JOIN aggregated_messages am
  ON am.message_id = mds.message_id AND am.mls_group_id = mds.mls_group_id
INNER JOIN accounts a ON a.pubkey = am.author;

-- 3. Replace old table.
DROP TABLE message_delivery_status;
ALTER TABLE message_delivery_status_v2 RENAME TO message_delivery_status;

-- 4. Recreate index for queries that filter by (account_pubkey, mls_group_id, status).
CREATE INDEX idx_mds_account_group_status
    ON message_delivery_status (account_pubkey, mls_group_id, status);

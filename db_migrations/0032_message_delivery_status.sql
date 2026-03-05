-- Track delivery status for outgoing messages in a dedicated table.
-- Decoupled from aggregated_messages to support future per-relay tracking.

CREATE TABLE message_delivery_status (
    message_id   TEXT NOT NULL,
    mls_group_id BLOB NOT NULL,
    status       TEXT NOT NULL,
    PRIMARY KEY (message_id, mls_group_id),
    FOREIGN KEY (message_id, mls_group_id)
        REFERENCES aggregated_messages(message_id, mls_group_id)
        ON DELETE CASCADE
);

-- Queries frequently filter by (mls_group_id, status), e.g. excluding Retried messages.
CREATE INDEX idx_mds_group_status
    ON message_delivery_status (mls_group_id, status);

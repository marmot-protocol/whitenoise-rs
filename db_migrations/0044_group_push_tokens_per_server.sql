DROP INDEX IF EXISTS idx_group_push_tokens_account_group_leaf;

CREATE UNIQUE INDEX idx_group_push_tokens_account_group_leaf_server
    ON group_push_tokens(account_pubkey, mls_group_id, leaf_index, server_pubkey);

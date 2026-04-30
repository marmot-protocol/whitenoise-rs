use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        5
    }

    fn description(&self) -> &'static str {
        "Recreate group_push_tokens with member_pubkey column"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        sqlx::query("DROP INDEX IF EXISTS idx_group_push_tokens_account_group_leaf")
            .execute(&mut *tx)
            .await?;

        sqlx::query("DROP INDEX IF EXISTS idx_group_push_tokens_account_group")
            .execute(&mut *tx)
            .await?;

        sqlx::query("DROP INDEX IF EXISTS idx_group_push_tokens_account_server")
            .execute(&mut *tx)
            .await?;

        sqlx::query("ALTER TABLE group_push_tokens RENAME TO group_push_tokens_old")
            .execute(&mut *tx)
            .await?;

        sqlx::query(
            "CREATE TABLE group_push_tokens (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey TEXT NOT NULL,
                mls_group_id BLOB NOT NULL,
                member_pubkey TEXT NOT NULL,
                leaf_index INTEGER NOT NULL CHECK (leaf_index >= 0),
                server_pubkey TEXT NOT NULL,
                relay_hint TEXT,
                encrypted_token TEXT NOT NULL CHECK (
                    length(trim(encrypted_token, ' ' || char(9) || char(10) || char(13))) > 0
                ),
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                FOREIGN KEY (account_pubkey) REFERENCES accounts(pubkey) ON DELETE CASCADE
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE UNIQUE INDEX idx_group_push_tokens_account_group_leaf
                ON group_push_tokens(account_pubkey, mls_group_id, leaf_index)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX idx_group_push_tokens_account_group
                ON group_push_tokens(account_pubkey, mls_group_id)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX idx_group_push_tokens_account_group_member
                ON group_push_tokens(account_pubkey, mls_group_id, member_pubkey)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX idx_group_push_tokens_account_server
                ON group_push_tokens(account_pubkey, server_pubkey)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query("DROP TABLE group_push_tokens_old")
            .execute(&mut *tx)
            .await?;

        Ok(())
    }
}

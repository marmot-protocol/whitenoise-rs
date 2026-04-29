use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        9
    }

    fn description(&self) -> &'static str {
        "Add NIP-51 mute list table"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        sqlx::query(
            "CREATE TABLE mute_list (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey TEXT NOT NULL,
                muted_pubkey   TEXT NOT NULL,
                is_private     INTEGER NOT NULL DEFAULT 1 CHECK (is_private IN (0, 1)),
                created_at     INTEGER NOT NULL,
                UNIQUE(account_pubkey, muted_pubkey)
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query("CREATE INDEX idx_mute_list_account ON mute_list(account_pubkey)")
            .execute(&mut *tx)
            .await?;

        Ok(())
    }
}

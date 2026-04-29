use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        6
    }

    fn description(&self) -> &'static str {
        "Add self_removed to accounts_groups"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        sqlx::query(
            "ALTER TABLE accounts_groups \
             ADD COLUMN self_removed INTEGER NOT NULL DEFAULT 0 \
             CHECK (self_removed IN (0, 1))",
        )
        .execute(&mut *tx)
        .await?;

        Ok(())
    }
}

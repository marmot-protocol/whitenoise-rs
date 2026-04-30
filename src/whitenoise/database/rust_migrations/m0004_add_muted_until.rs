use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        4
    }

    fn description(&self) -> &'static str {
        "Add muted_until to accounts_groups"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        sqlx::query("ALTER TABLE accounts_groups ADD COLUMN muted_until INTEGER DEFAULT NULL")
            .execute(&mut *tx)
            .await?;

        Ok(())
    }
}

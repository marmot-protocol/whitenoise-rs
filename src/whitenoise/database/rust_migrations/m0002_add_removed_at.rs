use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        2
    }

    fn description(&self) -> &'static str {
        "Add removed_at to accounts_groups"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        sqlx::query("ALTER TABLE accounts_groups ADD COLUMN removed_at INTEGER DEFAULT NULL")
            .execute(&mut *tx)
            .await?;

        Ok(())
    }
}

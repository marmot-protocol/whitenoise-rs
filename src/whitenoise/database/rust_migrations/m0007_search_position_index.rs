use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        7
    }

    fn description(&self) -> &'static str {
        "Replace aggregated_messages kind-group index with message_id tiebreaker"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        sqlx::query("DROP INDEX IF EXISTS idx_aggregated_messages_kind_group")
            .execute(&mut *tx)
            .await?;

        sqlx::query(
            "CREATE INDEX idx_aggregated_messages_kind_group
                ON aggregated_messages(kind, mls_group_id, created_at DESC, message_id DESC)",
        )
        .execute(&mut *tx)
        .await?;

        Ok(())
    }
}

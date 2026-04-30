use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        11
    }

    fn description(&self) -> &'static str {
        "Add account_pubkey to delivery status primary key"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        sqlx::query(
            "CREATE TABLE message_delivery_status_new (
                message_id      TEXT NOT NULL,
                mls_group_id    BLOB NOT NULL,
                account_pubkey  TEXT NOT NULL,
                status          TEXT NOT NULL,
                PRIMARY KEY (message_id, mls_group_id, account_pubkey),
                FOREIGN KEY (message_id, mls_group_id)
                    REFERENCES aggregated_messages(message_id, mls_group_id)
                    ON DELETE CASCADE
            )",
        )
        .execute(&mut *tx)
        .await?;

        // Backfill: attribute existing rows to the message author.
        sqlx::query(
            "INSERT INTO message_delivery_status_new \
                (message_id, mls_group_id, account_pubkey, status)
            SELECT
                mds.message_id,
                mds.mls_group_id,
                am.author,
                mds.status
            FROM message_delivery_status mds
            INNER JOIN aggregated_messages am
                ON am.message_id = mds.message_id
               AND am.mls_group_id = mds.mls_group_id
            WHERE am.author IS NOT NULL",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query("DROP TABLE message_delivery_status")
            .execute(&mut *tx)
            .await?;

        sqlx::query("ALTER TABLE message_delivery_status_new RENAME TO message_delivery_status")
            .execute(&mut *tx)
            .await?;

        sqlx::query(
            "CREATE INDEX idx_mds_group_account_status
                ON message_delivery_status (mls_group_id, account_pubkey, status)",
        )
        .execute(&mut *tx)
        .await?;

        Ok(())
    }
}

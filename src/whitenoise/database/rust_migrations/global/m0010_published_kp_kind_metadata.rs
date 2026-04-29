use async_trait::async_trait;
use sqlx::SqliteConnection;

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::GlobalMigration;

pub struct Migration;

#[async_trait]
impl GlobalMigration for Migration {
    fn version(&self) -> u32 {
        10
    }

    fn description(&self) -> &'static str {
        "Add kind and d_tag columns to published_key_packages"
    }

    async fn run_global(&self, tx: &mut SqliteConnection) -> Result<(), DatabaseError> {
        sqlx::query(
            "ALTER TABLE published_key_packages ADD COLUMN kind INTEGER NOT NULL DEFAULT 443",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query("ALTER TABLE published_key_packages ADD COLUMN d_tag TEXT NULL")
            .execute(&mut *tx)
            .await?;

        sqlx::query(
            "CREATE INDEX idx_published_kp_account_hash_ref
                ON published_key_packages(account_pubkey, key_package_hash_ref)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX idx_published_kp_account_kind_d_tag
                ON published_key_packages(account_pubkey, kind, d_tag)",
        )
        .execute(&mut *tx)
        .await?;

        Ok(())
    }
}

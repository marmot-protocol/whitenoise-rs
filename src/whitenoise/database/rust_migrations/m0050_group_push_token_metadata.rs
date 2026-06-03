use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        50
    }

    fn description(&self) -> &'static str {
        "Add Darkmatter push-token metadata"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        _global_db: &SqlitePool,
        _account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        if !group_push_tokens_has_column(tx, "platform").await? {
            sqlx::query(
                "ALTER TABLE group_push_tokens
                 ADD COLUMN platform TEXT CHECK (platform IN ('apns', 'fcm'))",
            )
            .execute(&mut *tx)
            .await?;
        }

        if !group_push_tokens_has_column(tx, "token_fingerprint").await? {
            sqlx::query(
                "ALTER TABLE group_push_tokens
                 ADD COLUMN token_fingerprint TEXT CHECK (
                     token_fingerprint IS NULL OR
                     token_fingerprint GLOB 'sha256:[0-9a-f]*'
                 )",
            )
            .execute(&mut *tx)
            .await?;
        }

        Ok(())
    }
}

async fn group_push_tokens_has_column(
    tx: &mut SqliteConnection,
    column: &str,
) -> Result<bool, DatabaseError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('group_push_tokens')
         WHERE name = ?",
    )
    .bind(column)
    .fetch_one(&mut *tx)
    .await?;

    Ok(count > 0)
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;
    use tempfile::TempDir;

    use super::*;
    use crate::whitenoise::database::rust_migrations::Migrator;

    async fn pools() -> (SqlitePool, SqlitePool, TempDir) {
        let dir = TempDir::new().unwrap();
        let local = SqlitePool::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("local.db").display()
        ))
        .await
        .unwrap();
        let global = SqlitePool::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("global.db").display()
        ))
        .await
        .unwrap();
        (local, global, dir)
    }

    #[tokio::test]
    async fn adds_optional_darkmatter_metadata_columns() {
        let (local, global, _dir) = pools().await;
        sqlx::query(
            "CREATE TABLE group_push_tokens (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                mls_group_id BLOB NOT NULL,
                member_pubkey TEXT NOT NULL,
                leaf_index INTEGER NOT NULL CHECK (leaf_index >= 0),
                server_pubkey TEXT NOT NULL,
                relay_hint TEXT,
                encrypted_token TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE(mls_group_id, leaf_index)
            )",
        )
        .execute(&local)
        .await
        .unwrap();

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &"aa".repeat(32))))
            .await
            .unwrap();

        let platform_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('group_push_tokens')
             WHERE name = 'platform'",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        let fingerprint_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('group_push_tokens')
             WHERE name = 'token_fingerprint'",
        )
        .fetch_one(&local)
        .await
        .unwrap();

        assert_eq!(platform_columns, 1);
        assert_eq!(fingerprint_columns, 1);
    }

    #[tokio::test]
    async fn skips_columns_that_already_exist() {
        let (local, global, _dir) = pools().await;
        sqlx::query(
            "CREATE TABLE group_push_tokens (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                mls_group_id BLOB NOT NULL,
                member_pubkey TEXT NOT NULL,
                leaf_index INTEGER NOT NULL CHECK (leaf_index >= 0),
                platform TEXT CHECK (platform IN ('apns', 'fcm')),
                token_fingerprint TEXT CHECK (
                    token_fingerprint IS NULL OR
                    token_fingerprint GLOB 'sha256:[0-9a-f]*'
                ),
                server_pubkey TEXT NOT NULL,
                relay_hint TEXT,
                encrypted_token TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE(mls_group_id, leaf_index)
            )",
        )
        .execute(&local)
        .await
        .unwrap();

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &"aa".repeat(32))))
            .await
            .unwrap();

        let platform_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('group_push_tokens')
             WHERE name = 'platform'",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        let fingerprint_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('group_push_tokens')
             WHERE name = 'token_fingerprint'",
        )
        .fetch_one(&local)
        .await
        .unwrap();

        assert_eq!(platform_columns, 1);
        assert_eq!(fingerprint_columns, 1);
    }
}

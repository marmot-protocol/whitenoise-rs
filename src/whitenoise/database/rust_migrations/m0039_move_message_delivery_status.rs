use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

/// Temporary carrier for rows read from the shared `message_delivery_status`
/// table during migration. Not used outside this module.
#[derive(sqlx::FromRow)]
struct SharedDeliveryStatusRow {
    message_id: String,
    mls_group_id: Vec<u8>,
    account_pubkey: String,
    status: String,
}

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        39
    }

    fn description(&self) -> &'static str {
        "Move message_delivery_status into per-account database"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        // Per-account schema: column set unchanged from m0011 so that existing
        // queries keep working without rewrites. The `account_pubkey` column
        // becomes redundant (every row in this DB belongs to the same account)
        // but is preserved as a no-op filter.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS message_delivery_status (
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

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_mds_group_account_status
                ON message_delivery_status (mls_group_id, account_pubkey, status)",
        )
        .execute(&mut *tx)
        .await?;

        let shared_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='message_delivery_status')",
        )
        .fetch_one(global_db)
        .await?;

        if !shared_table_exists {
            tracing::warn!(
                target: "whitenoise::database::rust_migrations::m0039",
                account = account_pubkey,
                "shared.message_delivery_status is missing; skipping local copy. \
                 Expected only on fresh installs or when v40 already dropped \
                 the table."
            );
            return Ok(());
        }

        let rows: Vec<SharedDeliveryStatusRow> = sqlx::query_as(
            "SELECT message_id, mls_group_id, account_pubkey, status
             FROM message_delivery_status
             WHERE account_pubkey = ?",
        )
        .bind(account_pubkey)
        .fetch_all(global_db)
        .await?;

        // The local FK on (message_id, mls_group_id) only fires when the
        // referenced row exists, so we gate inserts on the presence of a
        // matching aggregated_messages row that v38 just copied. Rows whose
        // parent message wasn't copied (e.g. for groups this account isn't
        // in) are silently skipped — they would never satisfy the FK.
        let mut inserted = 0u64;
        for r in &rows {
            let parent_present: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM aggregated_messages
                  WHERE message_id = ? AND mls_group_id = ?)",
            )
            .bind(&r.message_id)
            .bind(&r.mls_group_id)
            .fetch_one(&mut *tx)
            .await?;

            if !parent_present {
                continue;
            }

            sqlx::query(
                "INSERT OR IGNORE INTO message_delivery_status
                    (message_id, mls_group_id, account_pubkey, status)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(&r.message_id)
            .bind(&r.mls_group_id)
            .bind(&r.account_pubkey)
            .bind(&r.status)
            .execute(&mut *tx)
            .await?;
            inserted += 1;
        }

        if inserted > 0 {
            tracing::info!(
                target: "whitenoise::database::rust_migrations::m0039",
                account = account_pubkey,
                "Copied {inserted} message_delivery_status row(s) to per-account DB"
            );
        }

        Ok(())
    }
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

    async fn seed_local_aggregated(local: &SqlitePool, message_ids: &[(&str, &[u8])]) {
        // Mirror the schema produced by m0038 (no FK to group_information).
        sqlx::query(
            "CREATE TABLE aggregated_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                message_id TEXT NOT NULL,
                mls_group_id BLOB NOT NULL,
                author TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                kind INTEGER NOT NULL,
                content TEXT NOT NULL DEFAULT '',
                tags JSONB NOT NULL,
                reply_to_id TEXT,
                deletion_event_id TEXT,
                content_tokens JSONB NOT NULL,
                reactions JSONB NOT NULL,
                media_attachments JSONB NOT NULL,
                content_normalized TEXT NOT NULL DEFAULT '',
                UNIQUE(message_id, mls_group_id)
            )",
        )
        .execute(local)
        .await
        .unwrap();
        let author = "d".repeat(64);
        for (msg, gid) in message_ids {
            sqlx::query(
                "INSERT INTO aggregated_messages
                    (message_id, mls_group_id, author, created_at, kind, content,
                     tags, content_tokens, reactions, media_attachments)
                 VALUES (?, ?, ?, 0, 9, '', '[]', '[]', '{}', '[]')",
            )
            .bind(*msg)
            .bind(*gid)
            .bind(&author)
            .execute(local)
            .await
            .unwrap();
        }
    }

    async fn seed_shared_mds(global: &SqlitePool) {
        sqlx::query(
            "CREATE TABLE message_delivery_status (
                message_id TEXT NOT NULL,
                mls_group_id BLOB NOT NULL,
                account_pubkey TEXT NOT NULL,
                status TEXT NOT NULL,
                PRIMARY KEY (message_id, mls_group_id, account_pubkey)
            )",
        )
        .execute(global)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn migration_copies_only_this_account_with_parent_present() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        let other_pubkey = "ff".repeat(32);

        seed_local_aggregated(
            &local,
            &[("1".repeat(64).as_str(), &[0x01, 0x02, 0x03, 0x04])],
        )
        .await;
        seed_shared_mds(&global).await;

        // Status for this account, parent present locally — copied.
        sqlx::query(
            "INSERT INTO message_delivery_status
                (message_id, mls_group_id, account_pubkey, status)
             VALUES (?, ?, ?, '\"Sent\"')",
        )
        .bind("1".repeat(64))
        .bind(&[0x01u8, 0x02, 0x03, 0x04][..])
        .bind(&pubkey)
        .execute(&global)
        .await
        .unwrap();

        // Status for this account but parent NOT present locally — skipped.
        sqlx::query(
            "INSERT INTO message_delivery_status
                (message_id, mls_group_id, account_pubkey, status)
             VALUES (?, ?, ?, '\"Sent\"')",
        )
        .bind("2".repeat(64))
        .bind(&[0x05u8, 0x06][..])
        .bind(&pubkey)
        .execute(&global)
        .await
        .unwrap();

        // Status for a different account — must not leak in.
        sqlx::query(
            "INSERT INTO message_delivery_status
                (message_id, mls_group_id, account_pubkey, status)
             VALUES (?, ?, ?, '\"Sent\"')",
        )
        .bind("1".repeat(64))
        .bind(&[0x01u8, 0x02, 0x03, 0x04][..])
        .bind(&other_pubkey)
        .execute(&global)
        .await
        .unwrap();

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM message_delivery_status")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(
            count, 1,
            "only this account's status with parent present is copied"
        );

        let (account,): (String,) =
            sqlx::query_as("SELECT account_pubkey FROM message_delivery_status")
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(account, pubkey);
    }

    #[tokio::test]
    async fn migration_tolerates_missing_shared_table() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ef".repeat(32);
        seed_local_aggregated(&local, &[]).await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='message_delivery_status')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);
    }
}

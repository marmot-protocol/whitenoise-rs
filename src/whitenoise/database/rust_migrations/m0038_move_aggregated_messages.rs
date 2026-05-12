use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

/// Temporary carrier for rows read from the shared `aggregated_messages` table
/// during migration. Not used outside this module.
#[derive(sqlx::FromRow)]
struct SharedAggregatedMessageRow {
    message_id: String,
    mls_group_id: Vec<u8>,
    author: String,
    created_at: i64,
    kind: i64,
    content: String,
    tags: String,
    reply_to_id: Option<String>,
    deletion_event_id: Option<String>,
    content_tokens: String,
    reactions: String,
    media_attachments: String,
    content_normalized: String,
}

pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        38
    }

    fn description(&self) -> &'static str {
        "Move aggregated_messages into per-account database"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        // Per-account schema: identical to shared but the cross-DB FK to
        // group_information(mls_group_id) is dropped (group_information is
        // shared, this table is account-scoped).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS aggregated_messages (
                id                    INTEGER PRIMARY KEY AUTOINCREMENT,
                message_id            TEXT NOT NULL
                    CHECK (length(message_id) = 64 AND message_id GLOB '[0-9a-fA-F]*'),
                mls_group_id          BLOB NOT NULL,
                author                TEXT NOT NULL
                    CHECK (length(author) = 64 AND author GLOB '[0-9a-fA-F]*'),
                created_at            INTEGER NOT NULL,
                kind                  INTEGER NOT NULL,
                content               TEXT NOT NULL DEFAULT '',
                tags                  JSONB NOT NULL,
                reply_to_id           TEXT
                    CHECK (reply_to_id IS NULL OR
                           (length(reply_to_id) = 64 AND reply_to_id GLOB '[0-9a-fA-F]*')),
                deletion_event_id     TEXT
                    CHECK (deletion_event_id IS NULL OR
                           (length(deletion_event_id) = 64
                            AND deletion_event_id GLOB '[0-9a-fA-F]*')),
                content_tokens        JSONB NOT NULL,
                reactions             JSONB NOT NULL,
                media_attachments     JSONB NOT NULL,
                content_normalized    TEXT NOT NULL DEFAULT '',
                UNIQUE(message_id, mls_group_id)
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_aggregated_messages_message_id
                ON aggregated_messages(message_id)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_aggregated_messages_group
                ON aggregated_messages(mls_group_id, created_at)",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_aggregated_messages_kind_group
                ON aggregated_messages(kind, mls_group_id, created_at DESC, message_id DESC)",
        )
        .execute(&mut *tx)
        .await?;

        let shared_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='aggregated_messages')",
        )
        .fetch_one(global_db)
        .await?;

        if !shared_table_exists {
            tracing::warn!(
                target: "whitenoise::database::rust_migrations::m0038",
                account = account_pubkey,
                "shared.aggregated_messages is missing; skipping local copy. \
                 Expected only on fresh installs or when v41 already dropped \
                 the table."
            );
            return Ok(());
        }

        // accounts_groups was moved to the per-account DB by v36, so the
        // membership lookup now happens locally.
        let group_ids: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT mls_group_id FROM accounts_groups")
                .fetch_all(&mut *tx)
                .await?;

        let mut copied = 0u64;
        for group_id in &group_ids {
            let rows: Vec<SharedAggregatedMessageRow> = sqlx::query_as(
                "SELECT message_id, mls_group_id, author, created_at, kind, content,
                        tags, reply_to_id, deletion_event_id, content_tokens, reactions,
                        media_attachments, content_normalized
                 FROM aggregated_messages
                 WHERE mls_group_id = ?",
            )
            .bind(group_id)
            .fetch_all(global_db)
            .await?;

            for r in &rows {
                sqlx::query(
                    "INSERT OR IGNORE INTO aggregated_messages
                        (message_id, mls_group_id, author, created_at, kind, content,
                         tags, reply_to_id, deletion_event_id, content_tokens, reactions,
                         media_attachments, content_normalized)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&r.message_id)
                .bind(&r.mls_group_id)
                .bind(&r.author)
                .bind(r.created_at)
                .bind(r.kind)
                .bind(&r.content)
                .bind(&r.tags)
                .bind(&r.reply_to_id)
                .bind(&r.deletion_event_id)
                .bind(&r.content_tokens)
                .bind(&r.reactions)
                .bind(&r.media_attachments)
                .bind(&r.content_normalized)
                .execute(&mut *tx)
                .await?;
            }
            copied += rows.len() as u64;
        }

        if copied > 0 {
            tracing::info!(
                target: "whitenoise::database::rust_migrations::m0038",
                account = account_pubkey,
                "Copied {copied} aggregated_messages row(s) across {} group(s) to per-account DB",
                group_ids.len()
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

    async fn seed_local_accounts_groups(local: &SqlitePool, group_ids: &[&[u8]]) {
        // Mirror the schema produced by m0036.
        sqlx::query(
            "CREATE TABLE accounts_groups (
                id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                mls_group_id         BLOB NOT NULL UNIQUE,
                user_confirmation    INTEGER DEFAULT NULL,
                welcomer_pubkey      TEXT,
                last_read_message_id TEXT,
                pin_order            INTEGER DEFAULT NULL,
                dm_peer_pubkey       TEXT DEFAULT NULL,
                archived_at          INTEGER DEFAULT NULL,
                removed_at           INTEGER DEFAULT NULL,
                self_removed         INTEGER NOT NULL DEFAULT 0,
                muted_until          INTEGER DEFAULT NULL,
                chat_cleared_at      INTEGER DEFAULT NULL,
                created_at           INTEGER NOT NULL,
                updated_at           INTEGER NOT NULL
            )",
        )
        .execute(local)
        .await
        .unwrap();

        for gid in group_ids {
            sqlx::query(
                "INSERT INTO accounts_groups (mls_group_id, created_at, updated_at)
                 VALUES (?, 0, 0)",
            )
            .bind(*gid)
            .execute(local)
            .await
            .unwrap();
        }
    }

    async fn seed_shared(global: &SqlitePool) {
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
        .execute(global)
        .await
        .unwrap();

        // Two messages in group A (which the account joins), one in group B
        // (which the account does NOT join — must not be copied).
        let ma = "a".repeat(64);
        let mb = "b".repeat(64);
        let mc = "c".repeat(64);
        let author = "d".repeat(64);

        for (msg, gid, ts) in [
            (&ma, &[0x01, 0x02, 0x03, 0x04][..], 1000),
            (&mb, &[0x01, 0x02, 0x03, 0x04][..], 2000),
            (&mc, &[0x99, 0x88][..], 3000),
        ] {
            sqlx::query(
                "INSERT INTO aggregated_messages
                    (message_id, mls_group_id, author, created_at, kind, content,
                     tags, content_tokens, reactions, media_attachments)
                 VALUES (?, ?, ?, ?, 9, '', '[]', '[]', '{}', '[]')",
            )
            .bind(msg)
            .bind(gid)
            .bind(&author)
            .bind(ts)
            .execute(global)
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn migration_copies_only_account_groups() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        seed_local_accounts_groups(&local, &[&[0x01, 0x02, 0x03, 0x04]]).await;
        seed_shared(&global).await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM aggregated_messages")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(
            count, 2,
            "only the two messages in the joined group are copied"
        );

        let (other_present,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM aggregated_messages WHERE mls_group_id = ?")
                .bind(&[0x99u8, 0x88][..])
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(
            other_present, 0,
            "non-joined group's messages must not leak in"
        );
    }

    #[tokio::test]
    async fn migration_creates_table_with_no_groups() {
        let (local, global, _dir) = pools().await;
        let pubkey = "cd".repeat(32);
        seed_local_accounts_groups(&local, &[]).await;
        seed_shared(&global).await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='aggregated_messages')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM aggregated_messages")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn migration_tolerates_missing_shared_table() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ef".repeat(32);
        seed_local_accounts_groups(&local, &[&[0x01, 0x02]]).await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='aggregated_messages')",
        )
        .fetch_one(&local)
        .await
        .unwrap();
        assert!(exists);
    }
}

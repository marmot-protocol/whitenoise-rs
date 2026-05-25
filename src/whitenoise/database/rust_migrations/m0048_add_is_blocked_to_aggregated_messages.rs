use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

/// Adds the per-message `is_blocked` marker to `aggregated_messages`.
///
/// The marker is frozen at ingest time: a message authored by a muted pubkey
/// is stamped `is_blocked = 1` so the frontend can hide it without a live
/// `mute_list` lookup, and the stamp survives a later unblock (the
/// forward-looking rule).
///
/// The backfill stamps existing rows whose author is currently muted **and**
/// whose `created_at` is at or after the mute-list event's `event_created_at`
/// (added by m0047). Pre-block messages stay `is_blocked = 0` so they remain
/// visible. Both `aggregated_messages.created_at` and
/// `mute_list.event_created_at` are Unix-millisecond timestamps, so the
/// comparison is well-defined.
///
/// `fresh_account_schema.sql` already declares `is_blocked`, so the ALTER is
/// guarded by a column-existence check — the column-level equivalent of
/// `CREATE TABLE IF NOT EXISTS`, since SQLite has no
/// `ADD COLUMN IF NOT EXISTS`. A fresh account also has no messages, so
/// skipping the backfill on that path is correct.
pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        48
    }

    fn description(&self) -> &'static str {
        "Add is_blocked marker to aggregated_messages"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        _global_db: &SqlitePool,
        _account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        let column_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('aggregated_messages') \
             WHERE name = 'is_blocked')",
        )
        .fetch_one(&mut *tx)
        .await?;

        if column_exists {
            // Fresh account: fresh_account_schema.sql already created the
            // column, and a fresh account has no messages to backfill.
            return Ok(());
        }

        sqlx::query(
            "ALTER TABLE aggregated_messages
             ADD COLUMN is_blocked INTEGER NOT NULL DEFAULT 0
                 CHECK (is_blocked IN (0, 1))",
        )
        .execute(&mut *tx)
        .await?;

        // Stamp messages received from currently-muted authors at or after
        // the block was recorded. `event_created_at` is deterministic across
        // devices, so every device that synced the same mute-list event
        // agrees on which messages are blocked.
        sqlx::query(
            "UPDATE aggregated_messages AS am
             SET is_blocked = 1
             WHERE am.is_blocked = 0
               AND EXISTS (
                   SELECT 1 FROM mute_list ml
                   WHERE ml.muted_pubkey = am.author
                     AND am.created_at >= ml.event_created_at
               )",
        )
        .execute(&mut *tx)
        .await?;

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

    /// Re-creates the post-m0047 `mute_list` shape (with `event_created_at`).
    async fn seed_mute_list(local: &SqlitePool) {
        sqlx::query(
            "CREATE TABLE mute_list (
                id                INTEGER PRIMARY KEY AUTOINCREMENT,
                muted_pubkey      TEXT NOT NULL UNIQUE,
                is_private        INTEGER NOT NULL DEFAULT 1,
                created_at        INTEGER NOT NULL,
                event_created_at  INTEGER NOT NULL
            )",
        )
        .execute(local)
        .await
        .unwrap();
    }

    async fn mute(local: &SqlitePool, muted_pubkey: &str, event_created_at: i64) {
        sqlx::query(
            "INSERT INTO mute_list (muted_pubkey, is_private, created_at, event_created_at)
             VALUES (?, 1, ?, ?)",
        )
        .bind(muted_pubkey)
        .bind(event_created_at)
        .bind(event_created_at)
        .execute(local)
        .await
        .unwrap();
    }

    /// Re-creates the pre-m0048 `aggregated_messages` shape (no `is_blocked`).
    async fn seed_pre_m0048_aggregated_messages(local: &SqlitePool) {
        sqlx::query(
            "CREATE TABLE aggregated_messages (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                message_id          TEXT NOT NULL,
                mls_group_id        BLOB NOT NULL,
                author              TEXT NOT NULL,
                created_at          INTEGER NOT NULL,
                kind                INTEGER NOT NULL,
                content             TEXT NOT NULL DEFAULT '',
                tags                JSONB NOT NULL,
                reply_to_id         TEXT,
                deletion_event_id   TEXT,
                content_tokens      JSONB NOT NULL,
                reactions           JSONB NOT NULL,
                media_attachments   JSONB NOT NULL,
                content_normalized  TEXT NOT NULL DEFAULT '',
                UNIQUE(message_id, mls_group_id)
            )",
        )
        .execute(local)
        .await
        .unwrap();
    }

    async fn insert_message(local: &SqlitePool, message_id: &str, author: &str, created_at: i64) {
        sqlx::query(
            "INSERT INTO aggregated_messages
                (message_id, mls_group_id, author, created_at, kind,
                 tags, content_tokens, reactions, media_attachments)
             VALUES (?, x'01', ?, ?, 9, '[]', '[]', '[]', '[]')",
        )
        .bind(message_id)
        .bind(author)
        .bind(created_at)
        .execute(local)
        .await
        .unwrap();
    }

    async fn is_blocked(local: &SqlitePool, message_id: &str) -> i64 {
        sqlx::query_scalar("SELECT is_blocked FROM aggregated_messages WHERE message_id = ?")
            .bind(message_id)
            .fetch_one(local)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn adds_is_blocked_column() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        seed_mute_list(&local).await;
        seed_pre_m0048_aggregated_messages(&local).await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(aggregated_messages)")
                .fetch_all(&local)
                .await
                .unwrap();
        let col = columns.iter().find(|c| c.1 == "is_blocked");
        assert!(col.is_some(), "is_blocked column missing");
        assert_eq!(col.unwrap().3, 1, "is_blocked must be NOT NULL");
    }

    #[tokio::test]
    async fn backfill_leaves_unmuted_authors_unblocked() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        let muted = "11".repeat(32);
        let innocent = "22".repeat(32);

        seed_mute_list(&local).await;
        mute(&local, &muted, 5000).await;
        seed_pre_m0048_aggregated_messages(&local).await;
        insert_message(&local, &"aa".repeat(32), &innocent, 9000).await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        assert_eq!(
            is_blocked(&local, &"aa".repeat(32)).await,
            0,
            "a message from an unmuted author must stay unblocked"
        );
    }

    #[tokio::test]
    async fn backfill_stamps_post_block_messages_from_muted_authors() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        let muted = "11".repeat(32);

        seed_mute_list(&local).await;
        mute(&local, &muted, 5000).await;
        seed_pre_m0048_aggregated_messages(&local).await;
        // created_at == event_created_at — the boundary case, must stamp.
        insert_message(&local, &"aa".repeat(32), &muted, 5000).await;
        insert_message(&local, &"bb".repeat(32), &muted, 9000).await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        assert_eq!(
            is_blocked(&local, &"aa".repeat(32)).await,
            1,
            "message at exactly event_created_at must be stamped"
        );
        assert_eq!(
            is_blocked(&local, &"bb".repeat(32)).await,
            1,
            "message after the block must be stamped"
        );
    }

    #[tokio::test]
    async fn backfill_leaves_pre_block_messages_visible() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        let muted = "11".repeat(32);

        seed_mute_list(&local).await;
        mute(&local, &muted, 5000).await;
        seed_pre_m0048_aggregated_messages(&local).await;
        // created_at strictly before event_created_at — must stay visible.
        insert_message(&local, &"aa".repeat(32), &muted, 4999).await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        assert_eq!(
            is_blocked(&local, &"aa".repeat(32)).await,
            0,
            "a message sent before the block must remain visible"
        );
    }

    #[tokio::test]
    async fn is_noop_when_column_already_present() {
        // Fresh-account path: fresh_account_schema.sql already declares
        // is_blocked. The guarded ALTER must not error.
        let (local, global, _dir) = pools().await;
        let pubkey = "ef".repeat(32);

        seed_mute_list(&local).await;
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
                is_blocked INTEGER NOT NULL DEFAULT 0 CHECK (is_blocked IN (0, 1)),
                UNIQUE(message_id, mls_group_id)
            )",
        )
        .execute(&local)
        .await
        .unwrap();

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .expect("migration must be a no-op when the column already exists");
    }
}

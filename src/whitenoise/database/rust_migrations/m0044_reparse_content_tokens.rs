use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

/// Rerun the markdown parser over every cached message body and rewrite
/// `aggregated_messages.content_tokens`.
///
/// `content_tokens` is a write-once snapshot of `whitenoise_markdown::parse`
/// computed at message-ingest time and never refreshed by the runtime sync
/// path. When the parser's output changes (e.g. a new bare-URL scheme is
/// recognized), historical rows keep the old AST. This migration recomputes
/// the AST for every existing kind-9 row from its still-stored `content`.
///
/// Only kind 9 rows are touched. Kind 7 (reactions) and kind 5 (deletions)
/// always store an empty `Document` at write time (see
/// `database/aggregated_messages.rs` insert paths); re-parsing their content
/// would change that invariant for no rendering benefit.
pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        44
    }

    fn description(&self) -> &'static str {
        "Reparse content_tokens with updated markdown parser"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        _global_db: &SqlitePool,
        account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, content FROM aggregated_messages WHERE kind = 9")
                .fetch_all(&mut *tx)
                .await?;

        let total = rows.len();
        for (id, content) in rows {
            let doc = whitenoise_markdown::parse(&content);
            let json = serde_json::to_string(&doc)
                .expect("whitenoise_markdown::Document serialization is infallible");
            sqlx::query("UPDATE aggregated_messages SET content_tokens = ? WHERE id = ?")
                .bind(json)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }

        if total > 0 {
            tracing::info!(
                target: "whitenoise::database::rust_migrations::m0044",
                account = account_pubkey,
                "Reparsed content_tokens for {total} kind-9 message(s)"
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

    /// Create the `aggregated_messages` table (matching the m0038 shape) and
    /// insert rows. Each row's `content_tokens` is the caller-supplied raw
    /// JSON string so tests can seed deliberately-stale snapshots.
    async fn seed_aggregated_messages(local: &SqlitePool, rows: &[(i64, &str, &str)]) {
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

        let author = "a".repeat(64);
        for (idx, (kind, content, tokens)) in rows.iter().enumerate() {
            let message_id = format!("{:064x}", idx);
            sqlx::query(
                "INSERT INTO aggregated_messages
                    (message_id, mls_group_id, author, created_at, kind, content,
                     tags, content_tokens, reactions, media_attachments)
                 VALUES (?, X'01020304', ?, 0, ?, ?, '[]', ?, '{}', '[]')",
            )
            .bind(&message_id)
            .bind(&author)
            .bind(*kind)
            .bind(*content)
            .bind(*tokens)
            .execute(local)
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn migration_reparses_kind_9_content_tokens() {
        let (local, global, _dir) = pools().await;

        // Stale AST: just a Text node (pre-bare-URL parser behaviour).
        let stale =
            r#"{"blocks":[{"Paragraph":{"inlines":[{"Text":"see https://example.com"}]}}]}"#;
        seed_aggregated_messages(
            &local,
            &[(9, "see https://example.com", stale), (7, "+", stale)],
        )
        .await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &"aa".repeat(32))))
            .await
            .unwrap();

        // Kind 9 row should now match what the current parser produces.
        let expected =
            serde_json::to_string(&whitenoise_markdown::parse("see https://example.com")).unwrap();
        let (tokens_9,): (String,) =
            sqlx::query_as("SELECT content_tokens FROM aggregated_messages WHERE kind = 9")
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(tokens_9, expected);
        assert_ne!(
            tokens_9, stale,
            "migration should have replaced the stale AST"
        );

        // Kind 7 row must be left exactly as it was.
        let (tokens_7,): (String,) =
            sqlx::query_as("SELECT content_tokens FROM aggregated_messages WHERE kind = 7")
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(tokens_7, stale, "kind-7 row must not be touched");

        // Audit trail stamps version 44 exactly once.
        let (audit_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version = 44")
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(audit_count, 1);
    }

    #[tokio::test]
    async fn migration_is_noop_on_empty_table() {
        let (local, global, _dir) = pools().await;
        seed_aggregated_messages(&local, &[]).await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &"bb".repeat(32))))
            .await
            .unwrap();

        let (audit_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version = 44")
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(
            audit_count, 1,
            "audit-trail entry stamped even when no rows"
        );
    }

    #[tokio::test]
    async fn migration_is_idempotent() {
        let (local, global, _dir) = pools().await;
        seed_aggregated_messages(&local, &[(9, "hello", r#"{"blocks":[]}"#)]).await;

        let migrator = Migrator::new(vec![], vec![Box::new(Migration)]);
        let acct = "cc".repeat(32);

        migrator.run(&global, Some((&local, &acct))).await.unwrap();
        let (tokens_first,): (String,) =
            sqlx::query_as("SELECT content_tokens FROM aggregated_messages WHERE kind = 9")
                .fetch_one(&local)
                .await
                .unwrap();

        // Second run should short-circuit on the audit-trail check inside the
        // migrator; the stored tokens must be unchanged.
        migrator.run(&global, Some((&local, &acct))).await.unwrap();
        let (tokens_second,): (String,) =
            sqlx::query_as("SELECT content_tokens FROM aggregated_messages WHERE kind = 9")
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(tokens_first, tokens_second);

        let (audit_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _rust_migrations WHERE version = 44")
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(audit_count, 1);
    }
}

use async_trait::async_trait;
use sqlx::{SqliteConnection, SqlitePool};

use crate::whitenoise::database::DatabaseError;
use crate::whitenoise::database::rust_migrations::LocalMigration;

/// Adds the kind-10000 mute-list event's own `created_at` to each `mute_list`
/// row, so the block/unblock backfill and sweeps can reason about block
/// boundaries with a value that is identical across devices.
///
/// `mute_list.created_at` is per-device insert time (`Utc::now()` in
/// [`MuteListEntry::insert`](crate::whitenoise::database::mute_list::MuteListEntry::insert))
/// and therefore can't be used as a cross-device reference. The new
/// `event_created_at` column carries the originating event's `created_at`.
///
/// Pre-existing rows are backfilled from `created_at` as the best available
/// proxy — better than zero, which would cause the m0048 `is_blocked`
/// backfill to stamp every historical message as blocked.
///
/// `fresh_account_schema.sql` already declares `event_created_at`, so fresh
/// accounts reach this migration with the column present. The ALTER is
/// therefore guarded by a column-existence check — the column-level
/// equivalent of `CREATE TABLE IF NOT EXISTS`, since SQLite has no
/// `ADD COLUMN IF NOT EXISTS`.
pub struct Migration;

#[async_trait]
impl LocalMigration for Migration {
    fn version(&self) -> u32 {
        47
    }

    fn description(&self) -> &'static str {
        "Add event_created_at to mute_list for cross-device backfill"
    }

    async fn run_local(
        &self,
        tx: &mut SqliteConnection,
        _global_db: &SqlitePool,
        _account_pubkey: &str,
    ) -> Result<(), DatabaseError> {
        let column_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('mute_list') \
             WHERE name = 'event_created_at')",
        )
        .fetch_one(&mut *tx)
        .await?;

        if column_exists {
            // Fresh account: fresh_account_schema.sql already created the
            // column. Nothing to add or backfill.
            return Ok(());
        }

        // SQLite has no `ALTER TABLE … ADD COLUMN … NOT NULL` without a
        // `DEFAULT`, and any `DEFAULT` set here would persist in the column's
        // schema metadata — diverging from `fresh_account_schema.sql` (which
        // declares the column with no default) and silently accepting future
        // omitted-bind inserts on upgraded accounts.
        //
        // Rebuilding the table is the only way to land an upgraded schema
        // byte-for-byte identical to the fresh one. `mute_list` is a small
        // per-account table with no foreign-key references, so the rebuild is
        // cheap and safe; the `INSERT … SELECT` carries `id` over verbatim so
        // any in-flight references remain valid, and uses the per-device
        // `created_at` as the best available `event_created_at` for rows that
        // pre-date the column.
        sqlx::query(
            "CREATE TABLE mute_list_new (
                id                INTEGER PRIMARY KEY AUTOINCREMENT,
                muted_pubkey      TEXT NOT NULL UNIQUE
                    CHECK (length(muted_pubkey) = 64
                           AND muted_pubkey GLOB '[0-9a-fA-F]*'),
                is_private        INTEGER NOT NULL DEFAULT 1
                    CHECK (is_private IN (0, 1)),
                created_at        INTEGER NOT NULL,
                event_created_at  INTEGER NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO mute_list_new \
                (id, muted_pubkey, is_private, created_at, event_created_at) \
             SELECT id, muted_pubkey, is_private, created_at, created_at \
             FROM mute_list",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query("DROP TABLE mute_list")
            .execute(&mut *tx)
            .await?;

        sqlx::query("ALTER TABLE mute_list_new RENAME TO mute_list")
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

    /// Re-creates the pre-m0047 local `mute_list` shape (byte-for-byte with
    /// `m0042_move_mute_list.rs`).
    async fn seed_pre_m0047_mute_list(local: &SqlitePool) {
        sqlx::query(
            "CREATE TABLE mute_list (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                muted_pubkey  TEXT NOT NULL UNIQUE
                    CHECK (length(muted_pubkey) = 64
                           AND muted_pubkey GLOB '[0-9a-fA-F]*'),
                is_private    INTEGER NOT NULL DEFAULT 1
                    CHECK (is_private IN (0, 1)),
                created_at    INTEGER NOT NULL
            )",
        )
        .execute(local)
        .await
        .unwrap();
    }

    /// Re-creates the post-m0047 `mute_list` shape, matching
    /// `fresh_account_schema.sql` — the shape a fresh account already has.
    async fn seed_post_m0047_mute_list(local: &SqlitePool) {
        sqlx::query(
            "CREATE TABLE mute_list (
                id                INTEGER PRIMARY KEY AUTOINCREMENT,
                muted_pubkey      TEXT NOT NULL UNIQUE
                    CHECK (length(muted_pubkey) = 64
                           AND muted_pubkey GLOB '[0-9a-fA-F]*'),
                is_private        INTEGER NOT NULL DEFAULT 1
                    CHECK (is_private IN (0, 1)),
                created_at        INTEGER NOT NULL,
                event_created_at  INTEGER NOT NULL
            )",
        )
        .execute(local)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn adds_event_created_at_column() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        seed_pre_m0047_mute_list(&local).await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(mute_list)")
                .fetch_all(&local)
                .await
                .unwrap();
        let event_col = columns.iter().find(|c| c.1 == "event_created_at");
        let event_col = event_col.unwrap_or_else(|| {
            panic!(
                "event_created_at column missing; got {:?}",
                columns.iter().map(|c| &c.1).collect::<Vec<_>>()
            )
        });
        assert_eq!(event_col.3, 1, "event_created_at must be NOT NULL");
        assert!(
            event_col.4.is_none(),
            "event_created_at must have no DEFAULT, to match `fresh_account_schema.sql` \
             and avoid silently accepting implicit-zero inserts on upgraded accounts; \
             got default {:?}",
            event_col.4
        );
    }

    #[tokio::test]
    async fn backfills_event_created_at_from_per_device_created_at() {
        let (local, global, _dir) = pools().await;
        let pubkey = "ab".repeat(32);
        let target_a = "11".repeat(32);
        let target_b = "22".repeat(32);

        seed_pre_m0047_mute_list(&local).await;
        sqlx::query(
            "INSERT INTO mute_list (muted_pubkey, is_private, created_at)
             VALUES (?, 1, 1000), (?, 0, 2000)",
        )
        .bind(&target_a)
        .bind(&target_b)
        .execute(&local)
        .await
        .unwrap();

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let row_a: (i64, i64) = sqlx::query_as(
            "SELECT created_at, event_created_at FROM mute_list WHERE muted_pubkey = ?",
        )
        .bind(&target_a)
        .fetch_one(&local)
        .await
        .unwrap();
        assert_eq!(
            row_a,
            (1000, 1000),
            "event_created_at must mirror created_at for pre-existing rows"
        );

        let row_b: (i64, i64) = sqlx::query_as(
            "SELECT created_at, event_created_at FROM mute_list WHERE muted_pubkey = ?",
        )
        .bind(&target_b)
        .fetch_one(&local)
        .await
        .unwrap();
        assert_eq!(row_b, (2000, 2000));
    }

    #[tokio::test]
    async fn empty_mute_list_survives_migration() {
        let (local, global, _dir) = pools().await;
        let pubkey = "cd".repeat(32);
        seed_pre_m0047_mute_list(&local).await;

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .unwrap();

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mute_list")
            .fetch_one(&local)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn is_noop_when_column_already_present() {
        // Fresh-account path: fresh_account_schema.sql already declares
        // event_created_at. The guarded ALTER must not error or clobber.
        let (local, global, _dir) = pools().await;
        let pubkey = "ef".repeat(32);
        let target = "11".repeat(32);

        seed_post_m0047_mute_list(&local).await;
        sqlx::query(
            "INSERT INTO mute_list (muted_pubkey, is_private, created_at, event_created_at)
             VALUES (?, 1, 1000, 5000)",
        )
        .bind(&target)
        .execute(&local)
        .await
        .unwrap();

        Migrator::new(vec![], vec![Box::new(Migration)])
            .run(&global, Some((&local, &pubkey)))
            .await
            .expect("migration must be a no-op when the column already exists");

        let (event_created_at,): (i64,) =
            sqlx::query_as("SELECT event_created_at FROM mute_list WHERE muted_pubkey = ?")
                .bind(&target)
                .fetch_one(&local)
                .await
                .unwrap();
        assert_eq!(
            event_created_at, 5000,
            "an existing event_created_at value must not be overwritten"
        );
    }
}

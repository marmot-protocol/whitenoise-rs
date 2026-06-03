use cgka_traits::storage::{StorageError, StorageResult};
use rusqlite::{OptionalExtension, params};

use super::result_ext::RusqliteResultExt;

struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

const MARMOT_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial_schema",
        sql: r#"
CREATE TABLE IF NOT EXISTS marmot_convergence_policies (
    group_id BLOB PRIMARY KEY,
    policy BLOB NOT NULL
);
"#,
    },
    Migration {
        version: 2,
        name: "account_device_signers",
        sql: r#"
CREATE TABLE IF NOT EXISTS marmot_account_device_signers (
    marmot_identity BLOB PRIMARY KEY,
    record BLOB NOT NULL
);
"#,
    },
    Migration {
        version: 3,
        name: "group_metadata",
        sql: r#"
CREATE TABLE IF NOT EXISTS marmot_groups (
    id BLOB PRIMARY KEY,
    epoch INTEGER NOT NULL,
    record BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS marmot_messages (
    insert_order INTEGER PRIMARY KEY AUTOINCREMENT,
    id BLOB NOT NULL UNIQUE,
    group_id BLOB NOT NULL,
    epoch INTEGER NOT NULL,
    state INTEGER NOT NULL,
    record BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_marmot_messages_group_epoch
    ON marmot_messages (group_id, epoch, insert_order);

CREATE TABLE IF NOT EXISTS marmot_queued_outbound (
    insert_order INTEGER PRIMARY KEY AUTOINCREMENT,
    id BLOB NOT NULL UNIQUE,
    group_id BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL,
    record BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_marmot_queued_outbound_group
    ON marmot_queued_outbound (group_id, insert_order);

CREATE TABLE IF NOT EXISTS marmot_welcomes (
    message_id BLOB PRIMARY KEY,
    group_id BLOB NOT NULL,
    record BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS marmot_features (
    feature TEXT PRIMARY KEY,
    requirement BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS marmot_member_capabilities (
    group_id BLOB NOT NULL,
    member_id BLOB NOT NULL,
    capabilities BLOB NOT NULL,
    PRIMARY KEY (group_id, member_id)
);

CREATE TABLE IF NOT EXISTS marmot_group_snapshots (
    group_id BLOB NOT NULL,
    name TEXT NOT NULL,
    snapshot BLOB NOT NULL,
    PRIMARY KEY (group_id, name)
);
"#,
    },
    Migration {
        version: 4,
        name: "openmls_values",
        sql: r#"
CREATE TABLE IF NOT EXISTS marmot_openmls_values (
    provider_version INTEGER NOT NULL,
    label BLOB NOT NULL,
    storage_key BLOB NOT NULL,
    group_key BLOB,
    value BLOB NOT NULL,
    PRIMARY KEY (provider_version, storage_key)
);
CREATE INDEX IF NOT EXISTS idx_marmot_openmls_values_group
    ON marmot_openmls_values (provider_version, group_key);
"#,
    },
    Migration {
        version: 5,
        name: "group_projection",
        sql: r#"
CREATE TABLE IF NOT EXISTS marmot_group_projections (
    group_id BLOB PRIMARY KEY,
    record BLOB NOT NULL
);
"#,
    },
];

pub(super) fn run_migrations(connection: &mut rusqlite::Connection) -> StorageResult<()> {
    ensure_migrations_are_ordered()?;
    connection
        .execute_batch(
            r#"
CREATE TABLE IF NOT EXISTS marmot_schema_migrations (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);
"#,
        )
        .storage()?;
    reject_unknown_future_migrations(connection)?;

    for migration in MARMOT_MIGRATIONS {
        run_migration(connection, migration)?;
    }

    Ok(())
}

fn ensure_migrations_are_ordered() -> StorageResult<()> {
    let mut previous = 0_i64;
    for migration in MARMOT_MIGRATIONS {
        if migration.version <= previous {
            return Err(StorageError::Backend(format!(
                "Marmot storage migrations must be strictly ordered: {previous} then {}",
                migration.version
            )));
        }
        previous = migration.version;
    }
    Ok(())
}

fn reject_unknown_future_migrations(connection: &rusqlite::Connection) -> StorageResult<()> {
    let latest_known = MARMOT_MIGRATIONS
        .last()
        .map(|migration| migration.version)
        .unwrap_or_default();
    let future_version: Option<i64> = connection
        .query_row(
            "SELECT version FROM marmot_schema_migrations
             WHERE version > ?1
             ORDER BY version
             LIMIT 1",
            params![latest_known],
            |row| row.get(0),
        )
        .optional()
        .storage()?;

    match future_version {
        Some(version) => Err(StorageError::Backend(format!(
            "Marmot storage was migrated by a newer WhiteNoise version: {version}"
        ))),
        None => Ok(()),
    }
}

fn run_migration(
    connection: &mut rusqlite::Connection,
    migration: &Migration,
) -> StorageResult<()> {
    let applied_name: Option<String> = connection
        .query_row(
            "SELECT name FROM marmot_schema_migrations WHERE version = ?1",
            params![migration.version],
            |row| row.get(0),
        )
        .optional()
        .storage()?;

    if let Some(applied_name) = applied_name {
        if applied_name != migration.name {
            return Err(StorageError::Backend(format!(
                "Marmot storage migration {} changed name from {applied_name} to {}",
                migration.version, migration.name
            )));
        }
        return Ok(());
    }

    let transaction = connection.transaction().storage()?;
    transaction.execute_batch(migration.sql).storage()?;
    transaction
        .execute(
            "INSERT INTO marmot_schema_migrations (version, name)
             VALUES (?1, ?2)",
            params![migration.version, migration.name],
        )
        .storage()?;
    transaction.commit().storage()
}

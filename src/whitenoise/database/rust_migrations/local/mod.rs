use std::sync::LazyLock;

use super::{LocalMigration, LocalMigrationRunner};

pub fn all_local_migrations() -> Vec<Box<dyn LocalMigration>> {
    vec![]
}

pub static LOCAL_RUST_MIGRATOR: LazyLock<LocalMigrationRunner> =
    LazyLock::new(|| LocalMigrationRunner::new(all_local_migrations()));

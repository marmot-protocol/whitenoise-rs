use std::sync::LazyLock;

use super::{GlobalMigration, GlobalMigrationRunner};

mod m0001_bridge;

pub fn all_global_migrations() -> Vec<Box<dyn GlobalMigration>> {
    vec![Box::new(m0001_bridge::Migration)]
}

pub static GLOBAL_RUST_MIGRATOR: LazyLock<GlobalMigrationRunner> =
    LazyLock::new(|| GlobalMigrationRunner::new(all_global_migrations()));

use std::sync::LazyLock;

use super::{GlobalMigration, GlobalMigrationRunner};

mod m0001_bridge;
mod m0002_add_removed_at;
mod m0003_push_notifications;
mod m0004_add_muted_until;
mod m0005_group_push_tokens_member_identity;
mod m0006_add_self_removed;
mod m0007_search_position_index;
mod m0008_add_chat_cleared_at;
mod m0009_mute_list;
mod m0010_published_kp_kind_metadata;
mod m0011_delivery_status_account_scope;

pub fn all_global_migrations() -> Vec<Box<dyn GlobalMigration>> {
    vec![
        Box::new(m0001_bridge::Migration),
        Box::new(m0002_add_removed_at::Migration),
        Box::new(m0003_push_notifications::Migration),
        Box::new(m0004_add_muted_until::Migration),
        Box::new(m0005_group_push_tokens_member_identity::Migration),
        Box::new(m0006_add_self_removed::Migration),
        Box::new(m0007_search_position_index::Migration),
        Box::new(m0008_add_chat_cleared_at::Migration),
        Box::new(m0009_mute_list::Migration),
        Box::new(m0010_published_kp_kind_metadata::Migration),
        Box::new(m0011_delivery_status_account_scope::Migration),
    ]
}

pub static GLOBAL_RUST_MIGRATOR: LazyLock<GlobalMigrationRunner> =
    LazyLock::new(|| GlobalMigrationRunner::new(all_global_migrations()));

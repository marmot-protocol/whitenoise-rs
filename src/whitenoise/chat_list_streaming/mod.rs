//! Chat List Streaming Module
//!
//! This module provides real-time chat list streaming capabilities.
//! It enables subscribers to receive live updates as groups are created
//! and messages arrive, without requiring polling.

mod types;

pub use types::{ChatListSubscription, ChatListUpdate, ChatListUpdateTrigger};

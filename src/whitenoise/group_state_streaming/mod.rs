//! Group State Streaming Module
//!
//! Real-time group state events for an open chat view. Per-group broadcast
//! channels emit [`GroupStateUpdate`]s when the subscribed account leaves or
//! is removed from a group. Chat-list streaming covers the same transitions
//! for the chat list; this stream serves consumers that subscribe per-group
//! (e.g. an open chat screen) and shouldn't have to also subscribe to the
//! full chat list to learn the chat went away.

mod manager;
mod types;

pub use manager::GroupStateStreamManager;
pub use types::{GroupStateSubscription, GroupStateUpdate};

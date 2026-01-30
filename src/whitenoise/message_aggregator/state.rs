//! State management for stateful message aggregation
//!
//! This module defines the structures and interfaces for future stateful
//! implementation that will support incremental updates and persistent state.

use std::collections::HashMap;
use std::time::SystemTime;
use mdk_core::prelude::*;
use serde::{Deserialize, Serialize};

use super::types::{ChatMessage, GroupStatistics, AggregatorConfig};

/// Internal state for a specific group's message aggregation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GroupState {
    /// Processed messages by message ID
    pub messages: HashMap<String, ChatMessage>,

    /// Last processed timestamp to support incremental updates
    pub last_processed_at: Option<Timestamp>,

    /// Statistics for monitoring and debugging
    pub stats: GroupStatistics,

    /// Version for state format migrations
    pub state_version: u32,

    /// Dirty flag to track when state needs persistence
    #[serde(skip)]
    pub needs_persistence: bool,
}

impl GroupState {
    /// Create a new empty group state
    pub fn new() -> Self {
        Self {
            messages: HashMap::new(),
            last_processed_at: None,
            stats: GroupStatistics {
                message_count: 0,
                reaction_count: 0,
                deleted_message_count: 0,
                memory_usage_bytes: 0,
                last_processed_at: None,
            },
            state_version: STATE_VERSION,
            needs_persistence: false,
        }
    }

    /// Update statistics based on current state
    pub fn update_statistics(&mut self) {
        self.stats.message_count = self.messages.len();
        self.stats.deleted_message_count = self.messages.values()
            .filter(|msg| msg.is_deleted)
            .count();

        self.stats.reaction_count = self.messages.values()
            .map(|msg| msg.reactions.user_reactions.len())
            .sum();

        // Rough memory usage calculation
        self.stats.memory_usage_bytes = self.messages.values()
            .map(|msg| estimate_message_size(msg))
            .sum();

        self.stats.last_processed_at = self.last_processed_at;
    }

    /// Mark this state as needing persistence
    pub fn mark_dirty(&mut self) {
        self.needs_persistence = true;
    }

    /// Check if this state format is compatible with current version
    pub fn is_compatible(&self) -> bool {
        self.state_version <= STATE_VERSION
    }
}

/// Per-group aggregator state with isolation guarantees
#[derive(Debug)]
pub(crate) struct AggregatorState {
    /// State per group, ensuring no cross-group contamination
    pub groups: HashMap<GroupId, GroupState>,

    /// Global configuration
    pub config: AggregatorConfig,

    /// Last cleanup timestamp
    pub last_cleanup: Option<SystemTime>,
}

impl AggregatorState {
    /// Create a new aggregator state with given configuration
    pub fn new(config: AggregatorConfig) -> Self {
        Self {
            groups: HashMap::new(),
            config,
            last_cleanup: None,
        }
    }

    /// Get or create state for a specific group
    pub fn get_or_create_group(&mut self, group_id: &GroupId) -> &mut GroupState {
        self.groups.entry(group_id.clone()).or_insert_with(GroupState::new)
    }

    /// Remove state for a specific group
    pub fn remove_group(&mut self, group_id: &GroupId) -> Option<GroupState> {
        self.groups.remove(group_id)
    }

    /// Get statistics for all groups
    pub fn get_all_statistics(&self) -> HashMap<GroupId, GroupStatistics> {
        self.groups.iter()
            .map(|(group_id, state)| (group_id.clone(), state.stats.clone()))
            .collect()
    }

    /// Find groups that need persistence
    pub fn groups_needing_persistence(&self) -> Vec<GroupId> {
        self.groups.iter()
            .filter(|(_, state)| state.needs_persistence)
            .map(|(group_id, _)| group_id.clone())
            .collect()
    }

    /// Mark all groups as persisted
    pub fn mark_all_persisted(&mut self) {
        for state in self.groups.values_mut() {
            state.needs_persistence = false;
        }
    }

    /// Clean up old or unused state
    pub fn cleanup_old_state(&mut self, max_age_days: u64) {
        let cutoff = SystemTime::now() - std::time::Duration::from_secs(max_age_days * 24 * 60 * 60);

        self.groups.retain(|_, state| {
            if let Some(last_processed) = state.last_processed_at {
                let last_processed_system_time = SystemTime::UNIX_EPOCH +
                    std::time::Duration::from_secs(last_processed.as_secs());
                last_processed_system_time > cutoff
            } else {
                // Keep state without last_processed_at to be safe
                true
            }
        });

        self.last_cleanup = Some(SystemTime::now());
    }
}

/// Current state format version
const STATE_VERSION: u32 = 1;

/// Estimate the memory usage of a ChatMessage
fn estimate_message_size(message: &ChatMessage) -> usize {
    let base_size = std::mem::size_of::<ChatMessage>();
    let content_size = message.content.len();
    let id_size = message.id.len();
    let reply_id_size = message.reply_to_id.as_ref().map(|s| s.len()).unwrap_or(0);

    // Estimate token size
    let tokens_size = message.content_tokens.len() * 32; // Rough estimate

    // Estimate reactions size
    let reactions_size = message.reactions.user_reactions.len() * 64 +
                        message.reactions.by_emoji.len() * 48;

    base_size + content_size + id_size + reply_id_size + tokens_size + reactions_size
}

/// Errors related to state management
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("Failed to serialize state: {0}")]
    SerializationFailed(String),

    #[error("Failed to deserialize state: {0}")]
    DeserializationFailed(String),

    #[error("State version incompatible: found {found}, expected <= {expected}")]
    IncompatibleVersion { found: u32, expected: u32 },

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Group not found: {0}")]
    GroupNotFound(String),
}

// Future APIs for stateful processing:

impl AggregatorState {
    /// Persist state for a specific group to disk
    pub async fn persist_group_state(&mut self, group_id: &GroupId, path: &std::path::Path) -> Result<(), StateError> {
        if let Some(state) = self.groups.get_mut(group_id) {
            // TODO: Add bincode dependency for serialization
            // let serialized = bincode::serialize(state)
            //     .map_err(|e| StateError::SerializationFailed(e.to_string()))?;

            let group_path = path.join(format!("group_{}.state", hex::encode(group_id.as_slice())));
            // tokio::fs::write(&group_path, serialized).await?;
            tokio::fs::write(&group_path, b"placeholder").await?; // Placeholder for now

            state.needs_persistence = false;
            Ok(())
        } else {
            Err(StateError::GroupNotFound(hex::encode(group_id.as_slice())))
        }
    }

    /// Load persisted state for a specific group from disk
    pub async fn load_group_state(&mut self, group_id: &GroupId, path: &std::path::Path) -> Result<(), StateError> {
        let group_path = path.join(format!("group_{}.state", hex::encode(group_id.as_slice())));

        if !group_path.exists() {
            // No state file exists, create new state
            self.groups.insert(group_id.clone(), GroupState::new());
            return Ok(());
        }

        // TODO: Add bincode dependency for deserialization
        // let serialized = tokio::fs::read(&group_path).await?;
        // let mut state: GroupState = bincode::deserialize(&serialized)
        //     .map_err(|e| StateError::DeserializationFailed(e.to_string()))?;

        // For now, just create a new state when file exists
        let mut state = GroupState::new();

        if !state.is_compatible() {
            return Err(StateError::IncompatibleVersion {
                found: state.state_version,
                expected: STATE_VERSION,
            });
        }

        // Reset transient fields
        state.needs_persistence = false;

        self.groups.insert(group_id.clone(), state);
        Ok(())
    }

    /// Clear all cached/persisted state for a specific group
    pub async fn clear_group_state(&mut self, group_id: &GroupId, path: &std::path::Path) -> Result<(), StateError> {
        // Remove from memory
        self.groups.remove(group_id);

        // Remove from disk
        let group_path = path.join(format!("group_{}.state", hex::encode(group_id.as_slice())));
        if group_path.exists() {
            tokio::fs::remove_file(&group_path).await?;
        }

        Ok(())
    }

    /// Clear all state for all groups
    pub async fn clear_all_state(&mut self, path: &std::path::Path) -> Result<(), StateError> {
        // Clear memory
        self.groups.clear();

        // Clear disk
        if path.exists() {
            let mut dir = tokio::fs::read_dir(path).await?;
            while let Some(entry) = dir.next_entry().await? {
                let file_path = entry.path();
                if let Some(file_name) = file_path.file_name() {
                    if file_name.to_string_lossy().ends_with(".state") {
                        tokio::fs::remove_file(&file_path).await?;
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_group_state_creation() {
        let state = GroupState::new();
        assert!(state.messages.is_empty());
        assert!(state.last_processed_at.is_none());
        assert_eq!(state.stats.message_count, 0);
        assert_eq!(state.state_version, STATE_VERSION);
        assert!(!state.needs_persistence);
    }

    #[test]
    fn test_aggregator_state_creation() {
        let config = AggregatorConfig::default();
        let state = AggregatorState::new(config);
        assert!(state.groups.is_empty());
        assert!(state.last_cleanup.is_none());
    }

    #[test]
    fn test_get_or_create_group() {
        let config = AggregatorConfig::default();
        let mut state = AggregatorState::new(config);

        let group_id = GroupId::from_slice(&[1; 32]).unwrap();

        let group_state = state.get_or_create_group(&group_id);
        assert_eq!(group_state.state_version, STATE_VERSION);

        // Should return the same instance on second call
        let group_state2 = state.get_or_create_group(&group_id);
        assert_eq!(group_state2.state_version, STATE_VERSION);

        assert_eq!(state.groups.len(), 1);
    }

    #[test]
    fn test_state_compatibility() {
        let state = GroupState::new();
        assert!(state.is_compatible());

        let mut old_state = GroupState::new();
        old_state.state_version = STATE_VERSION + 1;
        assert!(!old_state.is_compatible());
    }

    #[tokio::test]
    async fn test_persist_and_load_group_state() {
        let temp_dir = TempDir::new().unwrap();
        let config = AggregatorConfig::default();
        let mut state = AggregatorState::new(config);

        let group_id = GroupId::from_slice(&[1; 32]).unwrap();

        // Create some state
        let group_state = state.get_or_create_group(&group_id);
        group_state.mark_dirty();

        // Persist it
        let result = state.persist_group_state(&group_id, temp_dir.path()).await;
        assert!(result.is_ok());

        // Clear and reload
        state.groups.clear();
        let result = state.load_group_state(&group_id, temp_dir.path()).await;
        assert!(result.is_ok());

        // Should have the group back
        assert!(state.groups.contains_key(&group_id));
        assert!(!state.groups.get(&group_id).unwrap().needs_persistence);
    }

    #[tokio::test]
    async fn test_clear_group_state() {
        let temp_dir = TempDir::new().unwrap();
        let config = AggregatorConfig::default();
        let mut state = AggregatorState::new(config);

        let group_id = GroupId::from_slice(&[1; 32]).unwrap();

        // Create and persist some state
        let group_state = state.get_or_create_group(&group_id);
        group_state.mark_dirty();
        let _ = state.persist_group_state(&group_id, temp_dir.path()).await;

        // Clear it
        let result = state.clear_group_state(&group_id, temp_dir.path()).await;
        assert!(result.is_ok());

        // Should be gone from memory
        assert!(!state.groups.contains_key(&group_id));

        // Should be gone from disk
        let group_path = temp_dir.path().join(format!("group_{}.state", hex::encode(group_id.as_slice())));
        assert!(!group_path.exists());
    }
}

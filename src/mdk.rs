//! Re-exports of [`mdk_core`] types used by the whitenoise public API.
//!
//! Consumers should depend only on `whitenoise` and access MLS protocol types
//! through this module, avoiding a direct `mdk-core` dependency and the
//! version-drift risk that comes with it.

// The fundamental group identifier used throughout the crate's public types.
pub use mdk_core::prelude::GroupId;

// MLS group model and state enum (fields of `GroupWithMembership`, etc.).
pub use mdk_core::prelude::group_types::{Group, GroupState};

// Configuration types required to create and update MLS groups.
pub use mdk_core::prelude::{NostrGroupConfigData, NostrGroupDataUpdate};

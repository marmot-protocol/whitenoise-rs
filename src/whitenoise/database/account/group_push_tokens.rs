//! Per-account repository for cached group push tokens.
//!
//! Wraps the existing [`GroupPushToken`] DB functions so that callers do not
//! need to thread an `account_pubkey` argument through every call — the pubkey
//! is baked in at construction time.

use std::collections::BTreeMap;
use std::sync::Arc;

use mdk_core::prelude::GroupId;
use nostr_sdk::{PublicKey, RelayUrl};

use crate::whitenoise::database::Database;
use crate::whitenoise::error::Result;
use crate::whitenoise::push_notifications::GroupPushToken;

/// Repository for group push tokens scoped to a single account.
#[derive(Clone, Debug)]
pub struct GroupPushTokensRepo {
    account_pubkey: PublicKey,
    db: Arc<Database>,
}

impl GroupPushTokensRepo {
    /// Construct a new [`GroupPushTokensRepo`] for `account_pubkey`.
    pub(crate) fn new(account_pubkey: PublicKey, db: Arc<Database>) -> Self {
        Self { account_pubkey, db }
    }

    /// Return all cached push tokens for a specific group.
    pub async fn find_by_group(&self, group_id: &GroupId) -> Result<Vec<GroupPushToken>> {
        Ok(
            GroupPushToken::find_by_account_and_group(&self.account_pubkey, group_id, &self.db)
                .await?,
        )
    }

    /// Insert or update a single cached push token for a group member.
    pub async fn upsert(
        &self,
        group_id: &GroupId,
        member_pubkey: &PublicKey,
        leaf_index: u32,
        server_pubkey: &PublicKey,
        relay_hint: Option<&RelayUrl>,
        encrypted_token: &str,
    ) -> Result<GroupPushToken> {
        Ok(GroupPushToken::upsert(
            &self.account_pubkey,
            group_id,
            member_pubkey,
            leaf_index,
            server_pubkey,
            relay_hint,
            encrypted_token,
            &self.db,
        )
        .await?)
    }

    /// Delete the cached token for a specific leaf index in a group.
    pub async fn delete(&self, group_id: &GroupId, leaf_index: u32) -> Result<bool> {
        Ok(GroupPushToken::delete(&self.account_pubkey, group_id, leaf_index, &self.db).await?)
    }

    /// Delete all cached tokens for a specific member in a group.
    pub async fn delete_by_member_pubkey(
        &self,
        group_id: &GroupId,
        member_pubkey: &PublicKey,
    ) -> Result<bool> {
        Ok(GroupPushToken::delete_by_member_pubkey(
            &self.account_pubkey,
            group_id,
            member_pubkey,
            &self.db,
        )
        .await?)
    }

    /// Bulk upsert tokens from a token-list response, filtering to active
    /// leaves only.
    pub async fn upsert_active_token_list_response(
        &self,
        group_id: &GroupId,
        active_leaf_map: &BTreeMap<u32, PublicKey>,
        tokens: Vec<mdk_core::mip05::LeafTokenTag>,
    ) -> Result<()> {
        Ok(GroupPushToken::upsert_active_token_list_response(
            &self.account_pubkey,
            group_id,
            active_leaf_map,
            tokens,
            &self.db,
        )
        .await?)
    }
}

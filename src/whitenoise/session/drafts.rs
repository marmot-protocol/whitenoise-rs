//! Draft operations scoped to an [`AccountSession`].

use mdk_core::prelude::GroupId;
use nostr_sdk::EventId;

use super::AccountSession;
use crate::whitenoise::drafts::Draft;
use crate::whitenoise::error::Result;
use crate::whitenoise::media_files::MediaFile;

/// View over [`AccountSession`] for draft message operations.
///
/// Obtain via [`AccountSession::drafts`].
pub struct DraftOps<'a> {
    session: &'a AccountSession,
}

impl<'a> DraftOps<'a> {
    pub(super) fn new(session: &'a AccountSession) -> Self {
        Self { session }
    }

    /// Upsert the draft for `(account, group_id)`.
    pub async fn save(
        &self,
        group_id: &GroupId,
        content: &str,
        reply_to_id: Option<&EventId>,
        media_attachments: &[MediaFile],
    ) -> Result<Draft> {
        self.session
            .repos
            .drafts
            .save(group_id, content, reply_to_id, media_attachments)
            .await
    }

    /// Load the draft for `(account, group_id)`, or `None` if absent.
    pub async fn load(&self, group_id: &GroupId) -> Result<Option<Draft>> {
        self.session.repos.drafts.find(group_id).await
    }

    /// Delete the draft for `(account, group_id)`. No-op if absent.
    pub async fn delete(&self, group_id: &GroupId) -> Result<()> {
        self.session.repos.drafts.delete(group_id).await
    }
}

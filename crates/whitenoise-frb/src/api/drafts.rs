use crate::api::wn_session;
use crate::api::{
    error::ApiError,
    media_files::MediaFile,
    utils::{group_id_from_string, group_id_to_string},
};
use chrono::{DateTime, Utc};
use flutter_rust_bridge::frb;
use nostr_sdk::prelude::*;
use whitenoise::{Draft as WhitenoiseDraft, MediaFile as WhitenoiseMediaFile};

/// Flutter-compatible draft message for a specific group.
///
/// Drafts preserve unsent message state (text content and reply context)
/// scoped to an account and group. Only one draft exists per (account, group) pair.
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct Draft {
    pub account_pubkey: String,
    pub mls_group_id: String,
    pub content: String,
    pub reply_to_id: Option<String>,
    pub media_attachments: Vec<MediaFile>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<WhitenoiseDraft> for Draft {
    fn from(draft: WhitenoiseDraft) -> Self {
        Self {
            account_pubkey: draft.account_pubkey.to_hex(),
            mls_group_id: group_id_to_string(&draft.mls_group_id),
            content: draft.content,
            reply_to_id: draft.reply_to_id.map(|id| id.to_hex()),
            media_attachments: draft
                .media_attachments
                .into_iter()
                .map(|mf| mf.into())
                .collect(),
            created_at: draft.created_at,
            updated_at: draft.updated_at,
        }
    }
}

/// Saves a draft for the given account and group.
///
/// Uses upsert semantics: creates a new draft or updates the existing one.
/// Only one draft is stored per (account, group) pair.
#[frb]
pub async fn save_draft(
    pubkey: String,
    group_id: String,
    content: String,
    reply_to_id: Option<String>,
    media_attachments: Vec<MediaFile>,
) -> Result<Draft, ApiError> {
    let pubkey = PublicKey::parse(&pubkey)?;
    let session = wn_session(&pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let reply_to = reply_to_id.as_deref().map(EventId::parse).transpose()?;
    let attachments = media_attachments
        .into_iter()
        .map(WhitenoiseMediaFile::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    let draft = session
        .drafts()
        .save(&group_id, &content, reply_to.as_ref(), &attachments)
        .await?;
    Ok(draft.into())
}

/// Loads the draft for the given account and group, if one exists.
#[frb]
pub async fn load_draft(pubkey: String, group_id: String) -> Result<Option<Draft>, ApiError> {
    let pubkey = PublicKey::parse(&pubkey)?;
    let session = wn_session(&pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let draft = session.drafts().load(&group_id).await?;
    Ok(draft.map(|d| d.into()))
}

/// Deletes the draft for the given account and group.
///
/// No-op if no draft exists.
#[frb]
pub async fn delete_draft(pubkey: String, group_id: String) -> Result<(), ApiError> {
    let pubkey = PublicKey::parse(&pubkey)?;
    let session = wn_session(&pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    session.drafts().delete(&group_id).await?;
    Ok(())
}

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use mdk_core::prelude::{GroupId, message_types::Message};
use nostr_sdk::prelude::*;

use super::{Database, DatabaseError, utils::parse_timestamp};
use crate::nostr_manager::parser::SerializableToken;
use crate::perf_instrument;
use crate::whitenoise::{
    aggregated_message::AggregatedMessage,
    media_files::MediaFile,
    message_aggregator::{
        ChatMessage, ChatMessageSummary, DeliveryStatus, ReactionSummary, SearchResult,
    },
    utils::timestamp_to_datetime,
};

type Result<T> = std::result::Result<T, DatabaseError>;

#[derive(Debug)]
struct AggregatedMessageRow {
    pub id: i64,
    pub message_id: EventId,
    pub mls_group_id: GroupId,
    pub author: PublicKey,
    pub created_at: DateTime<Utc>,
    pub kind: Kind,
    pub content: String,
    pub tags: Tags,
    pub reply_to_id: Option<EventId>,
    pub deletion_event_id: Option<EventId>,
    pub content_tokens: Vec<SerializableToken>,
    pub reactions: ReactionSummary,
    pub media_attachments: Vec<MediaFile>,
    pub delivery_status: Option<DeliveryStatus>,
}

impl<'r, R> sqlx::FromRow<'r, R> for AggregatedMessageRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Vec<u8>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> std::result::Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;

        // Convert message_id from hex string to EventId
        let message_id_hex: String = row.try_get("message_id")?;
        let message_id =
            EventId::from_hex(&message_id_hex).map_err(|e| sqlx::Error::ColumnDecode {
                index: "message_id".to_string(),
                source: Box::new(e),
            })?;

        // Convert mls_group_id from bytes to GroupId
        let mls_group_id_bytes: Vec<u8> = row.try_get("mls_group_id")?;
        let mls_group_id = GroupId::from_slice(&mls_group_id_bytes);

        // Convert author from hex string to PublicKey
        let author_hex: String = row.try_get("author")?;
        let author = PublicKey::from_hex(&author_hex).map_err(|e| sqlx::Error::ColumnDecode {
            index: "author".to_string(),
            source: Box::new(e),
        })?;

        // Convert created_at from milliseconds to DateTime<Utc>
        let created_at = parse_timestamp(row, "created_at")?;

        // Convert kind from i64 to Kind
        let kind_i64: i64 = row.try_get("kind")?;
        let kind = Kind::from(kind_i64 as u16);

        let content: String = row.try_get("content")?;

        // Deserialize tags from JSON string
        let tags_str: String = row.try_get("tags")?;
        let tags = serde_json::from_str(&tags_str).map_err(|e| sqlx::Error::ColumnDecode {
            index: "tags".to_string(),
            source: Box::new(e),
        })?;

        // Convert optional reply_to_id from hex string to EventId
        let reply_to_id = match row.try_get::<Option<String>, _>("reply_to_id")? {
            Some(hex) => Some(
                EventId::from_hex(&hex).map_err(|e| sqlx::Error::ColumnDecode {
                    index: "reply_to_id".to_string(),
                    source: Box::new(e),
                })?,
            ),
            None => None,
        };

        // Convert optional deletion_event_id from hex string to EventId
        let deletion_event_id = match row.try_get::<Option<String>, _>("deletion_event_id")? {
            Some(hex) => Some(
                EventId::from_hex(&hex).map_err(|e| sqlx::Error::ColumnDecode {
                    index: "deletion_event_id".to_string(),
                    source: Box::new(e),
                })?,
            ),
            None => None,
        };

        // Deserialize JSONB fields from JSON strings
        let content_tokens_str: String = row.try_get("content_tokens")?;
        let content_tokens =
            serde_json::from_str(&content_tokens_str).map_err(|e| sqlx::Error::ColumnDecode {
                index: "content_tokens".to_string(),
                source: Box::new(e),
            })?;

        let reactions_str: String = row.try_get("reactions")?;
        let reactions =
            serde_json::from_str(&reactions_str).map_err(|e| sqlx::Error::ColumnDecode {
                index: "reactions".to_string(),
                source: Box::new(e),
            })?;

        let media_attachments_str: String = row.try_get("media_attachments")?;
        let media_attachments = serde_json::from_str(&media_attachments_str).map_err(|e| {
            sqlx::Error::ColumnDecode {
                index: "media_attachments".to_string(),
                source: Box::new(e),
            }
        })?;

        // Deserialize optional delivery_status from JSON string.
        // Uses lenient ColumnNotFound handling because delivery_status lives in a
        // separate table and is only present when the query includes a LEFT JOIN alias.
        let delivery_status: Option<DeliveryStatus> =
            match row.try_get::<Option<String>, _>("delivery_status") {
                Ok(Some(json)) => {
                    serde_json::from_str(&json).map_err(|e| sqlx::Error::ColumnDecode {
                        index: "delivery_status".to_string(),
                        source: Box::new(e),
                    })?
                }
                Ok(None) => None,
                Err(sqlx::Error::ColumnNotFound(_)) => None,
                Err(e) => return Err(e),
            };

        Ok(Self {
            id,
            message_id,
            mls_group_id,
            author,
            created_at,
            kind,
            content,
            tags,
            reply_to_id,
            deletion_event_id,
            content_tokens,
            reactions,
            media_attachments,
            delivery_status,
        })
    }
}

impl AggregatedMessageRow {
    /// Convert database row to lightweight AggregatedMessage domain type
    fn into_aggregated_message(self) -> AggregatedMessage {
        AggregatedMessage {
            id: self.id,
            event_id: self.message_id,
            mls_group_id: self.mls_group_id,
            author: self.author,
            content: self.content,
            created_at: self.created_at,
            tags: self.tags,
        }
    }
}

impl AggregatedMessage {
    const DELIVERY_STATUS_LOCK_RETRY_DELAYS_MS: [u64; 3] = [25, 50, 100];

    /// Count ALL events (kind 9, 7, 5) in cache for a group
    /// Used for sync checking: mdk.len() == cache.len()
    #[perf_instrument("db::aggregated_messages")]
    pub async fn count_by_group(group_id: &GroupId, database: &Database) -> Result<usize> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM aggregated_messages WHERE mls_group_id = ?")
                .bind(group_id.as_slice())
                .fetch_one(&database.pool)
                .await?;

        Ok(count as usize)
    }

    /// Get ALL event IDs (all kinds) for a group
    /// Used for incremental sync: filter out cached events
    #[perf_instrument("db::aggregated_messages")]
    pub async fn get_all_event_ids_by_group(
        group_id: &GroupId,
        database: &Database,
    ) -> Result<HashSet<String>> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT message_id FROM aggregated_messages WHERE mls_group_id = ?")
                .bind(group_id.as_slice())
                .fetch_all(&database.pool)
                .await?;

        Ok(ids.into_iter().collect())
    }

    /// Fetch ALL kind 9 messages for a group (no pagination).
    ///
    /// Used internally by unit tests and low-level sync helpers.
    /// The main consumer-facing API uses `find_messages_by_group_paginated` instead.
    ///
    /// Query uses covering index: idx_aggregated_messages_kind_group(kind, mls_group_id, created_at)
    #[perf_instrument("db::aggregated_messages")]
    pub async fn find_messages_by_group(
        group_id: &GroupId,
        database: &Database,
    ) -> Result<Vec<ChatMessage>> {
        let rows: Vec<AggregatedMessageRow> = sqlx::query_as(
            "SELECT am.*, mds.status AS delivery_status
             FROM aggregated_messages am
             LEFT JOIN message_delivery_status mds
               ON am.message_id = mds.message_id AND am.mls_group_id = mds.mls_group_id
             WHERE am.kind = 9 AND am.mls_group_id = ?
               AND (mds.status IS NULL OR mds.status != '\"Retried\"')
             ORDER BY am.created_at",
        )
        .bind(group_id.as_slice())
        .fetch_all(&database.pool)
        .await?;

        rows.into_iter().map(Self::row_to_chat_message).collect()
    }

    /// Fetch ONLY kind 9 messages for a group with stable cursor-based pagination.
    ///
    /// Returns up to `limit` messages ordered oldest-first using a **compound cursor**
    /// `(created_at, message_id)` so that ties at the same second are deterministic and
    /// no message is skipped or repeated across pages.
    ///
    /// # Pagination protocol
    ///
    /// * Initial load: pass `before = None`, `before_message_id = None`.
    /// * Subsequent pages: pass the `created_at` **and** `id` of the *oldest* message
    ///   in the current page as `before` and `before_message_id` respectively.
    ///
    /// Both cursor fields must be provided together.  Supplying only one half returns
    /// `Err(DatabaseError::InvalidCursor)`.
    ///
    /// # Arguments
    /// * `group_id`          – the MLS group to query
    /// * `database`          – database handle
    /// * `before`            – cursor timestamp (Unix seconds); `None` means no upper bound
    /// * `before_message_id` – cursor message ID (hex); must be `Some` when `before` is `Some`
    /// * `limit`             – maximum rows to return (default 50, capped at 200)
    ///
    /// Query uses covering index: idx_aggregated_messages_kind_group(kind, mls_group_id, created_at)
    #[perf_instrument("db::aggregated_messages")]
    pub async fn find_messages_by_group_paginated(
        group_id: &GroupId,
        database: &Database,
        before: Option<Timestamp>,
        before_message_id: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<ChatMessage>> {
        // Clamp limit: default 50, max 200.
        let limit_val = i64::from(limit.unwrap_or(50).min(200));

        // Validate and resolve the compound before-cursor.
        //
        // Both fields must be present together or both absent.  A half-specified cursor
        // (one Some, one None) is rejected immediately rather than falling back to lossy
        // single-field logic that silently skips messages at a tied timestamp.
        //
        // The DB stores created_at as Unix milliseconds (i64); Timestamp is Unix seconds,
        // so we multiply by 1000.  When the cursor is absent we use i64::MAX as a sentinel
        // so that `created_at < MAX` is unconditionally true and the single query branch
        // still uses the covering index.
        let (before_ms, before_id_str): (i64, String) = match (before, before_message_id) {
            // No cursor: use i64::MAX so `created_at < MAX` is unconditionally true for every
            // real timestamp, returning the newest page without a cursor filter.
            //
            // The SQL predicate is:
            //   AND (am.created_at < ? OR (am.created_at = ? AND am.message_id < ?))
            //
            // With before_ms = i64::MAX and before_id_str = "", the second conjunct becomes
            // `am.message_id < ''`, which is always false because no hex string is
            // lexicographically less than the empty string in SQLite.  That is intentional:
            // the first OR branch (`created_at < i64::MAX`) already matches every row whose
            // timestamp is a realistic value, so the second conjunct is unreachable and its
            // falseness is harmless.  Both bind slots must still be filled to satisfy sqlx's
            // parameter count, hence the empty string placeholder.
            (None, None) => (i64::MAX, String::new()),
            (Some(ts), Some(id)) => {
                // Canonicalize to lowercase hex so the lexicographic tie-break comparison
                // against stored message_ids (always lowercase) is stable regardless of the
                // case the caller used.  An invalid or wrong-length ID is rejected here
                // rather than silently producing an incorrect page boundary.
                let canonical = EventId::from_hex(id).map(|eid| eid.to_hex()).map_err(|_| {
                    DatabaseError::InvalidCursor {
                        reason: "before_message_id is not a valid 64-character hex event ID",
                    }
                })?;
                ((ts.as_secs() as i64).saturating_mul(1_000), canonical)
            }
            (Some(_), None) => {
                return Err(DatabaseError::InvalidCursor {
                    reason: "before_message_id is required when before is provided",
                });
            }
            (None, Some(_)) => {
                return Err(DatabaseError::InvalidCursor {
                    reason: "before is required when before_message_id is provided",
                });
            }
        };

        let rows: Vec<AggregatedMessageRow> = sqlx::query_as(
            "SELECT am.*, mds.status AS delivery_status
             FROM aggregated_messages am
             LEFT JOIN message_delivery_status mds
               ON am.message_id = mds.message_id AND am.mls_group_id = mds.mls_group_id
             WHERE am.kind = 9 AND am.mls_group_id = ?
               AND (am.created_at < ? OR (am.created_at = ? AND am.message_id < ?))
               AND (mds.status IS NULL OR mds.status != '\"Retried\"')
             ORDER BY am.created_at DESC, am.message_id DESC
             LIMIT ?",
        )
        .bind(group_id.as_slice())
        .bind(before_ms)
        .bind(before_ms)
        .bind(before_id_str)
        .bind(limit_val)
        .fetch_all(&database.pool)
        .await?;

        // Rows arrive newest-first (DESC, DESC); reverse to restore oldest-first for callers.
        let mut messages: Vec<ChatMessage> = rows
            .into_iter()
            .map(Self::row_to_chat_message)
            .collect::<Result<Vec<_>>>()?;
        messages.reverse();
        Ok(messages)
    }

    /// Search messages within a group by content using forward-order substring matching.
    ///
    /// Matches against `content_normalized`, a NFC-lowercased copy of the content
    /// stored at insert time — so that case folding is correct for all Unicode scripts,
    /// including those where SQLite's built-in `LOWER()` is a no-op.
    ///
    /// Each returned [`SearchResult`] includes the matched [`ChatMessage`] and
    /// `highlight_spans`: char-index `[start, end]` pairs for each query token in the
    /// order they appear in the message content, ready for frontend highlighting.
    pub async fn search_messages_in_group(
        group_id: &GroupId,
        query: &str,
        limit: u32,
        database: &Database,
    ) -> Result<Vec<SearchResult>> {
        let limit_val = i64::from(limit.min(200));
        let like_pattern = super::content_search::query_to_like_pattern(query);

        let rows: Vec<AggregatedMessageRow> = sqlx::query_as(
            "SELECT am.*, mds.status AS delivery_status
             FROM aggregated_messages am
             LEFT JOIN message_delivery_status mds
               ON am.message_id = mds.message_id AND am.mls_group_id = mds.mls_group_id
             WHERE am.kind = 9
               AND am.mls_group_id = ?
               AND am.deletion_event_id IS NULL
               AND (mds.status IS NULL OR mds.status != '\"Retried\"')
               AND am.content_normalized LIKE ?
             ORDER BY am.created_at DESC
             LIMIT ?",
        )
        .bind(group_id.as_slice())
        .bind(&like_pattern)
        .bind(limit_val)
        .fetch_all(&database.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let message = Self::row_to_chat_message(row)?;
                let highlight_spans =
                    super::content_search::find_highlight_spans(&message.content, query);
                Ok(SearchResult {
                    message,
                    highlight_spans,
                })
            })
            .collect()
    }

    /// Save all events (kind 9, 7, 5) from sync in ONE transaction with single batch INSERT
    ///
    /// All events inserted in one batch - kind 9 gets full data, kind 7/5 get empty defaults
    /// Single pass - no UPDATE needed. This ensures atomicity: either all events are saved or none are
    #[perf_instrument("db::aggregated_messages")]
    pub async fn save_events(
        events: Vec<Message>,                 // All events (kind 9, 7, 5)
        processed_messages: Vec<ChatMessage>, // Processed kind 9 with aggregated data
        group_id: &GroupId,
        database: &Database,
    ) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let mut tx = database.pool.begin().await?;

        // Build a map for quick lookup of processed messages
        let processed_map: std::collections::HashMap<String, &ChatMessage> = processed_messages
            .iter()
            .map(|msg| (msg.id.clone(), msg))
            .collect();

        // Empty defaults for kind 7/5 events
        let empty_tokens = Vec::<SerializableToken>::new();
        let empty_reactions = ReactionSummary::default();
        let empty_media = Vec::<MediaFile>::new();

        // Insert each event individually (SQLite doesn't support multi-value INSERT with JSONB)
        for message in &events {
            let created_at = timestamp_to_datetime(message.created_at).map_err(|_| {
                DatabaseError::InvalidTimestamp {
                    timestamp: message.created_at.as_secs() as i64,
                }
            })?;

            match message.kind {
                Kind::Custom(9) => {
                    // Kind 9: Get processed message data
                    let chat_msg = processed_map
                        .get(&message.id.to_string())
                        .ok_or_else(|| DatabaseError::Sqlx(sqlx::Error::RowNotFound))?;

                    sqlx::query(
                        "INSERT OR IGNORE INTO aggregated_messages
                         (message_id, mls_group_id, author, created_at, kind, content,
                          content_normalized, tags, reply_to_id, content_tokens, reactions,
                          media_attachments)
                         VALUES (?, ?, ?, ?, 9, ?, ?, ?, ?, ?, ?, ?)",
                    )
                    .bind(message.id.to_string())
                    .bind(group_id.as_slice())
                    .bind(message.pubkey.to_hex())
                    .bind(created_at.timestamp_millis())
                    .bind(&message.content)
                    .bind(super::content_search::normalize_for_search(
                        &message.content,
                    ))
                    .bind(serde_json::to_string(&message.tags)?)
                    .bind(chat_msg.reply_to_id.as_ref())
                    .bind(serde_json::to_string(&chat_msg.content_tokens)?)
                    .bind(serde_json::to_string(&chat_msg.reactions)?)
                    .bind(serde_json::to_string(&chat_msg.media_attachments)?)
                    .execute(&mut *tx)
                    .await?;
                }
                _ => {
                    // Kind 7/5: Use empty defaults
                    sqlx::query(
                        "INSERT OR IGNORE INTO aggregated_messages
                         (message_id, mls_group_id, author, created_at, kind, content, tags,
                          reply_to_id, content_tokens, reactions, media_attachments)
                         VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?)",
                    )
                    .bind(message.id.to_string())
                    .bind(group_id.as_slice())
                    .bind(message.pubkey.to_hex())
                    .bind(created_at.timestamp_millis())
                    .bind(u16::from(message.kind) as i64)
                    .bind(&message.content)
                    .bind(serde_json::to_string(&message.tags)?)
                    .bind(serde_json::to_string(&empty_tokens)?)
                    .bind(serde_json::to_string(&empty_reactions)?)
                    .bind(serde_json::to_string(&empty_media)?)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }

        tx.commit().await?;
        Ok(())
    }

    /// Insert a single kind 9 message with full pre-aggregated data
    /// Used by event processor for real-time caching
    #[perf_instrument("db::aggregated_messages")]
    pub async fn insert_message(
        message: &ChatMessage,
        group_id: &GroupId,
        database: &Database,
    ) -> Result<()> {
        let created_at = timestamp_to_datetime(message.created_at).map_err(|_| {
            DatabaseError::InvalidTimestamp {
                timestamp: message.created_at.as_secs() as i64,
            }
        })?;

        let mut tx = database.pool.begin().await?;

        sqlx::query(
            "INSERT INTO aggregated_messages
             (message_id, mls_group_id, author, created_at, kind, content, content_normalized,
              tags, reply_to_id, content_tokens, reactions, media_attachments)
             VALUES (?, ?, ?, ?, 9, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(message_id, mls_group_id) DO UPDATE SET
               content = excluded.content,
               content_normalized = excluded.content_normalized,
               tags = excluded.tags,
               reply_to_id = excluded.reply_to_id,
               content_tokens = excluded.content_tokens,
               reactions = excluded.reactions,
               media_attachments = excluded.media_attachments",
        )
        .bind(&message.id)
        .bind(group_id.as_slice())
        .bind(message.author.to_hex())
        .bind(created_at.timestamp_millis())
        .bind(&message.content)
        .bind(super::content_search::normalize_for_search(
            &message.content,
        ))
        .bind(serde_json::to_string(&message.tags)?)
        .bind(&message.reply_to_id)
        .bind(serde_json::to_string(&message.content_tokens)?)
        .bind(serde_json::to_string(&message.reactions)?)
        .bind(serde_json::to_string(&message.media_attachments)?)
        .execute(&mut *tx)
        .await?;

        if let Some(status) = &message.delivery_status {
            sqlx::query(
                "INSERT INTO message_delivery_status (message_id, mls_group_id, status)
                 VALUES (?, ?, ?)
                 ON CONFLICT(message_id, mls_group_id) DO UPDATE SET status = excluded.status",
            )
            .bind(&message.id)
            .bind(group_id.as_slice())
            .bind(serde_json::to_string(status)?)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        Ok(())
    }

    /// Insert a kind 7 reaction event (audit trail)
    #[perf_instrument("db::aggregated_messages")]
    pub async fn insert_reaction(
        reaction: &Message,
        group_id: &GroupId,
        database: &Database,
    ) -> Result<()> {
        let created_at = timestamp_to_datetime(reaction.created_at).map_err(|_| {
            DatabaseError::InvalidTimestamp {
                timestamp: reaction.created_at.as_secs() as i64,
            }
        })?;

        let empty_tokens = Vec::<SerializableToken>::new();
        let empty_reactions = ReactionSummary::default();
        let empty_media = Vec::<MediaFile>::new();

        sqlx::query(
            "INSERT INTO aggregated_messages
             (message_id, mls_group_id, author, created_at, kind, content, tags,
              content_tokens, reactions, media_attachments)
             VALUES (?, ?, ?, ?, 7, ?, ?, ?, ?, ?)
             ON CONFLICT(message_id, mls_group_id) DO NOTHING",
        )
        .bind(reaction.id.to_string())
        .bind(group_id.as_slice())
        .bind(reaction.pubkey.to_hex())
        .bind(created_at.timestamp_millis())
        .bind(&reaction.content)
        .bind(serde_json::to_string(&reaction.tags)?)
        .bind(serde_json::to_string(&empty_tokens)?)
        .bind(serde_json::to_string(&empty_reactions)?)
        .bind(serde_json::to_string(&empty_media)?)
        .execute(&database.pool)
        .await?;

        Ok(())
    }

    /// Insert a kind 5 deletion event (audit trail)
    #[perf_instrument("db::aggregated_messages")]
    pub async fn insert_deletion(
        deletion: &Message,
        group_id: &GroupId,
        database: &Database,
    ) -> Result<()> {
        let created_at = timestamp_to_datetime(deletion.created_at).map_err(|_| {
            DatabaseError::InvalidTimestamp {
                timestamp: deletion.created_at.as_secs() as i64,
            }
        })?;

        let empty_tokens = Vec::<SerializableToken>::new();
        let empty_reactions = ReactionSummary::default();
        let empty_media = Vec::<MediaFile>::new();

        sqlx::query(
            "INSERT INTO aggregated_messages
             (message_id, mls_group_id, author, created_at, kind, content, tags,
              content_tokens, reactions, media_attachments)
             VALUES (?, ?, ?, ?, 5, '', ?, ?, ?, ?)
             ON CONFLICT(message_id, mls_group_id) DO NOTHING",
        )
        .bind(deletion.id.to_string())
        .bind(group_id.as_slice())
        .bind(deletion.pubkey.to_hex())
        .bind(created_at.timestamp_millis())
        .bind(serde_json::to_string(&deletion.tags)?)
        .bind(serde_json::to_string(&empty_tokens)?)
        .bind(serde_json::to_string(&empty_reactions)?)
        .bind(serde_json::to_string(&empty_media)?)
        .execute(&database.pool)
        .await?;

        Ok(())
    }

    /// Update a kind 9 message's reaction summary
    #[perf_instrument("db::aggregated_messages")]
    pub async fn update_reactions(
        message_id: &str,
        group_id: &GroupId,
        reactions: &ReactionSummary,
        database: &Database,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE aggregated_messages
             SET reactions = ?
             WHERE message_id = ? AND mls_group_id = ? AND kind = 9",
        )
        .bind(serde_json::to_string(reactions)?)
        .bind(message_id)
        .bind(group_id.as_slice())
        .execute(&database.pool)
        .await?;

        Ok(())
    }

    /// Mark a message or reaction as deleted
    #[perf_instrument("db::aggregated_messages")]
    pub async fn mark_deleted(
        message_id: &str,
        group_id: &GroupId,
        deletion_event_id: &str,
        database: &Database,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE aggregated_messages
             SET deletion_event_id = ?
             WHERE message_id = ? AND mls_group_id = ? AND kind IN (7, 9)",
        )
        .bind(deletion_event_id)
        .bind(message_id)
        .bind(group_id.as_slice())
        .execute(&database.pool)
        .await?;

        Ok(())
    }

    /// Reverse a deletion by clearing `deletion_event_id` for targets of a specific deletion.
    ///
    /// Used to cascade delivery failure: if a kind-5 deletion fails to publish,
    /// we undo its effect on the target messages.
    #[perf_instrument("db::aggregated_messages")]
    pub async fn unmark_deleted(
        deletion_event_id: &str,
        group_id: &GroupId,
        database: &Database,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE aggregated_messages
             SET deletion_event_id = NULL
             WHERE deletion_event_id = ? AND mls_group_id = ?",
        )
        .bind(deletion_event_id)
        .bind(group_id.as_slice())
        .execute(&database.pool)
        .await?;

        Ok(())
    }

    /// Update the delivery status of a cached message and return the full updated message.
    ///
    /// Upserts into the separate `message_delivery_status` table, then fetches the
    /// full message via LEFT JOIN. Runs in a transaction for atomicity.
    ///
    /// Returns an error if no matching message was found.
    #[perf_instrument("db::aggregated_messages")]
    pub async fn update_delivery_status(
        message_id: &str,
        group_id: &GroupId,
        status: &DeliveryStatus,
        database: &Database,
    ) -> Result<ChatMessage> {
        let mut tx = database.pool.begin().await?;

        // Verify the parent message exists before upserting delivery status
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM aggregated_messages
             WHERE message_id = ? AND mls_group_id = ?)",
        )
        .bind(message_id)
        .bind(group_id.as_slice())
        .fetch_one(&mut *tx)
        .await?;

        if !exists {
            return Err(DatabaseError::Sqlx(sqlx::Error::RowNotFound));
        }

        // Upsert delivery status
        sqlx::query(
            "INSERT INTO message_delivery_status (message_id, mls_group_id, status)
             VALUES (?, ?, ?)
             ON CONFLICT(message_id, mls_group_id) DO UPDATE SET status = excluded.status",
        )
        .bind(message_id)
        .bind(group_id.as_slice())
        .bind(serde_json::to_string(status)?)
        .execute(&mut *tx)
        .await?;

        // Fetch the full message with updated status
        let row: Option<AggregatedMessageRow> = sqlx::query_as(
            "SELECT am.*, mds.status AS delivery_status
             FROM aggregated_messages am
             LEFT JOIN message_delivery_status mds
               ON am.message_id = mds.message_id AND am.mls_group_id = mds.mls_group_id
             WHERE am.message_id = ? AND am.mls_group_id = ?",
        )
        .bind(message_id)
        .bind(group_id.as_slice())
        .fetch_optional(&mut *tx)
        .await?;

        tx.commit().await?;

        match row {
            Some(r) => Self::row_to_chat_message(r),
            None => Err(DatabaseError::Sqlx(sqlx::Error::RowNotFound)),
        }
    }

    #[perf_instrument("db::aggregated_messages")]
    pub async fn update_delivery_status_with_retry(
        message_id: &str,
        group_id: &GroupId,
        status: &DeliveryStatus,
        database: &Database,
    ) -> Result<ChatMessage> {
        for (attempt, delay_ms) in Self::DELIVERY_STATUS_LOCK_RETRY_DELAYS_MS
            .iter()
            .copied()
            .enumerate()
        {
            match Self::update_delivery_status(message_id, group_id, status, database).await {
                Ok(message) => return Ok(message),
                Err(error) if Self::is_database_lock_error(&error) => {
                    tracing::debug!(
                        attempt = attempt + 1,
                        delay_ms,
                        message_id,
                        "Retrying delivery-status update after SQLite lock"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                Err(error) => return Err(error),
            }
        }

        Self::update_delivery_status(message_id, group_id, status, database).await
    }

    /// Insert an initial delivery status row for an outgoing event.
    ///
    /// Uses a single INSERT (no transaction) to avoid write contention with
    /// `publish_with_retries` which may be running concurrently for other events.
    /// Only suitable when the parent `aggregated_messages` row was just inserted.
    #[perf_instrument("db::aggregated_messages")]
    pub async fn insert_delivery_status(
        message_id: &str,
        group_id: &GroupId,
        status: &DeliveryStatus,
        database: &Database,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO message_delivery_status (message_id, mls_group_id, status)
             VALUES (?, ?, ?)
             ON CONFLICT(message_id, mls_group_id) DO UPDATE SET status = excluded.status",
        )
        .bind(message_id)
        .bind(group_id.as_slice())
        .bind(serde_json::to_string(status)?)
        .execute(&database.pool)
        .await?;

        Ok(())
    }

    /// Check whether an event has a delivery status row (i.e. was sent by us).
    #[perf_instrument("db::aggregated_messages")]
    pub async fn has_delivery_status(
        message_id: &str,
        group_id: &GroupId,
        database: &Database,
    ) -> Result<bool> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM message_delivery_status
             WHERE message_id = ? AND mls_group_id = ?)",
        )
        .bind(message_id)
        .bind(group_id.as_slice())
        .fetch_one(&database.pool)
        .await?;

        Ok(exists)
    }

    fn is_database_lock_error(error: &DatabaseError) -> bool {
        matches!(error, DatabaseError::Sqlx(sqlx::Error::Database(db_error))
            if db_error.message().contains("database is locked")
                || matches!(db_error.code().as_deref(), Some("5") | Some("6")))
    }

    /// Delete ALL cached events for a group
    #[perf_instrument("db::aggregated_messages")]
    pub async fn delete_by_group(group_id: &GroupId, database: &Database) -> Result<()> {
        sqlx::query("DELETE FROM aggregated_messages WHERE mls_group_id = ?")
            .bind(group_id.as_slice())
            .execute(&database.pool)
            .await?;
        Ok(())
    }

    /// Find a cached message by ID (for updating with reactions/deletions)
    #[perf_instrument("db::aggregated_messages")]
    pub async fn find_by_id(
        message_id: &str,
        group_id: &GroupId,
        database: &Database,
    ) -> Result<Option<ChatMessage>> {
        let row: Option<AggregatedMessageRow> = sqlx::query_as(
            "SELECT am.*, mds.status AS delivery_status
             FROM aggregated_messages am
             LEFT JOIN message_delivery_status mds
               ON am.message_id = mds.message_id AND am.mls_group_id = mds.mls_group_id
             WHERE am.message_id = ? AND am.mls_group_id = ? AND am.kind = 9",
        )
        .bind(message_id)
        .bind(group_id.as_slice())
        .fetch_optional(&database.pool)
        .await?;

        row.map(Self::row_to_chat_message).transpose()
    }

    /// Find a message by its EventId only (without requiring group_id).
    /// Returns the lightweight AggregatedMessage with mls_group_id for lookup purposes.
    #[perf_instrument("db::aggregated_messages")]
    pub async fn find_by_message_id(
        message_id: &EventId,
        database: &Database,
    ) -> Result<Option<AggregatedMessage>> {
        let row: Option<AggregatedMessageRow> =
            sqlx::query_as("SELECT * FROM aggregated_messages WHERE message_id = ? AND kind = 9")
                .bind(message_id.to_hex())
                .fetch_optional(&database.pool)
                .await?;

        Ok(row.map(AggregatedMessageRow::into_aggregated_message))
    }

    /// Count unread messages for a group given its read marker message ID.
    ///
    /// If no read marker is provided, returns total non-deleted message count.
    /// If read marker message doesn't exist, returns total count (safe fallback).
    #[perf_instrument("db::aggregated_messages")]
    pub async fn count_unread_for_group(
        group_id: &GroupId,
        read_marker: Option<&EventId>,
        database: &Database,
    ) -> Result<usize> {
        let count: i64 = match read_marker {
            Some(message_id) => {
                // Count messages after the read marker's timestamp
                sqlx::query_scalar(
                    "SELECT COUNT(*) FROM aggregated_messages am
                     WHERE am.mls_group_id = ?
                       AND am.kind = 9
                       AND am.deletion_event_id IS NULL
                       AND am.created_at > COALESCE(
                           (SELECT created_at FROM aggregated_messages
                            WHERE message_id = ? AND mls_group_id = ?),
                           0
                       )",
                )
                .bind(group_id.as_slice())
                .bind(message_id.to_hex())
                .bind(group_id.as_slice())
                .fetch_one(&database.pool)
                .await?
            }
            None => {
                // No read marker = all messages are unread
                sqlx::query_scalar(
                    "SELECT COUNT(*) FROM aggregated_messages
                     WHERE mls_group_id = ? AND kind = 9 AND deletion_event_id IS NULL",
                )
                .bind(group_id.as_slice())
                .fetch_one(&database.pool)
                .await?
            }
        };

        Ok(count as usize)
    }

    /// Count unread messages for multiple groups in a single batch query.
    ///
    /// Takes a slice of (group_id, optional_read_marker) pairs and returns a map
    /// of group_id -> unread_count. Groups with no messages return 0.
    #[perf_instrument("db::aggregated_messages")]
    pub async fn count_unread_for_groups(
        group_markers: &[(GroupId, Option<EventId>)],
        database: &Database,
    ) -> Result<HashMap<GroupId, usize>> {
        use sqlx::Row;

        if group_markers.is_empty() {
            return Ok(HashMap::new());
        }

        let groups_with_markers: Vec<_> = group_markers
            .iter()
            .filter_map(|(gid, marker)| marker.as_ref().map(|m| (gid, m)))
            .collect();

        let all_group_ids: Vec<Vec<u8>> = group_markers.iter().map(|(g, _)| g.to_vec()).collect();

        let mut qb: sqlx::QueryBuilder<sqlx::Sqlite> =
            sqlx::QueryBuilder::new("WITH marker_input AS (");

        // Build UNION ALL for marker input, or an empty-result query if no markers
        if groups_with_markers.is_empty() {
            qb.push("SELECT NULL AS group_id, NULL AS marker_id WHERE 0");
        } else {
            for (i, (group_id, marker_id)) in groups_with_markers.iter().enumerate() {
                if i > 0 {
                    qb.push(" UNION ALL ");
                }
                qb.push("SELECT ");
                qb.push_bind(group_id.as_slice());
                qb.push(" AS group_id, ");
                qb.push_bind(marker_id.to_hex());
                qb.push(" AS marker_id");
            }
        }

        qb.push(
            "), marker_timestamps AS ( \
                 SELECT mi.group_id, am.created_at \
                 FROM marker_input mi \
                 JOIN aggregated_messages am \
                   ON am.mls_group_id = mi.group_id \
                  AND am.message_id = mi.marker_id \
             ) \
             SELECT am.mls_group_id, COUNT(*) as count \
             FROM aggregated_messages am \
             LEFT JOIN marker_timestamps mt ON am.mls_group_id = mt.group_id \
             WHERE am.mls_group_id IN (",
        );

        let mut sep = qb.separated(", ");
        for group_id in &all_group_ids {
            sep.push_bind(group_id);
        }
        sep.push_unseparated(
            ") AND am.kind = 9 \
             AND am.deletion_event_id IS NULL \
             AND am.created_at > COALESCE(mt.created_at, 0) \
             GROUP BY am.mls_group_id",
        );

        let rows = qb.build().fetch_all(&database.pool).await?;

        // Parse results into HashMap
        let mut results: HashMap<GroupId, usize> = HashMap::new();
        for row in rows {
            let group_id_bytes: Vec<u8> = row.try_get("mls_group_id")?;
            let group_id = GroupId::from_slice(&group_id_bytes);
            let count: i64 = row.try_get("count")?;
            results.insert(group_id, count as usize);
        }

        // Ensure all input groups are represented (groups with no messages get 0)
        for (group_id, _) in group_markers {
            results.entry(group_id.clone()).or_insert(0);
        }

        Ok(results)
    }

    /// Find a cached reaction (kind 7) by its event ID
    /// Only returns reactions that haven't been deleted yet
    #[perf_instrument("db::aggregated_messages")]
    pub async fn find_reaction_by_id(
        message_id: &str,
        group_id: &GroupId,
        database: &Database,
    ) -> Result<Option<AggregatedMessage>> {
        let row: Option<AggregatedMessageRow> = sqlx::query_as(
            "SELECT * FROM aggregated_messages
             WHERE message_id = ? AND mls_group_id = ? AND kind = 7
               AND deletion_event_id IS NULL",
        )
        .bind(message_id)
        .bind(group_id.as_slice())
        .fetch_optional(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(row.map(AggregatedMessageRow::into_aggregated_message))
    }

    /// Find orphaned reactions targeting a specific message
    /// Returns reactions (kind 7) that reference the target message_id
    /// Uses json_each to properly parse the tags array
    #[perf_instrument("db::aggregated_messages")]
    pub async fn find_orphaned_reactions(
        message_id: &str,
        group_id: &GroupId,
        database: &Database,
    ) -> Result<Vec<AggregatedMessage>> {
        let rows: Vec<AggregatedMessageRow> = sqlx::query_as(
            "SELECT am.* FROM aggregated_messages am
             WHERE am.kind = 7
               AND am.mls_group_id = ?
               AND am.deletion_event_id IS NULL
               AND EXISTS (
                 SELECT 1 FROM json_each(am.tags) AS tag
                 WHERE json_extract(tag.value, '$[0]') = 'e'
                   AND json_extract(tag.value, '$[1]') = ?
               )",
        )
        .bind(group_id.as_slice())
        .bind(message_id)
        .fetch_all(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(rows
            .into_iter()
            .map(AggregatedMessageRow::into_aggregated_message)
            .collect())
    }

    /// Find orphaned deletions targeting a specific message
    /// Returns the event IDs of deletions (kind 5) that reference the target message_id
    /// Uses json_each to properly parse the tags array
    #[perf_instrument("db::aggregated_messages")]
    pub async fn find_orphaned_deletions(
        message_id: &str,
        group_id: &GroupId,
        database: &Database,
    ) -> Result<Vec<EventId>> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT am.message_id FROM aggregated_messages am
             WHERE am.kind = 5
               AND am.mls_group_id = ?
               AND EXISTS (
                 SELECT 1 FROM json_each(am.tags) AS tag
                 WHERE json_extract(tag.value, '$[0]') = 'e'
                   AND json_extract(tag.value, '$[1]') = ?
               )",
        )
        .bind(group_id.as_slice())
        .bind(message_id)
        .fetch_all(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(ids
            .into_iter()
            .filter_map(|id| EventId::from_hex(&id).ok())
            .collect())
    }

    /// Fetches the most recent kind-9 message for each group in a single query.
    ///
    /// Returns `ChatMessageSummary` with `author_display_name: None`.
    /// The caller populates display names after a separate user batch lookup.
    ///
    /// Groups without messages or with only deleted messages are not included
    /// in the result.
    #[perf_instrument("db::aggregated_messages")]
    pub async fn find_last_by_group_ids(
        group_ids: &[GroupId],
        database: &Database,
    ) -> Result<Vec<ChatMessageSummary>> {
        use sqlx::Row;

        if group_ids.is_empty() {
            return Ok(Vec::new());
        }

        let group_id_bytes: Vec<Vec<u8>> = group_ids.iter().map(|id| id.to_vec()).collect();

        // Correlated subquery to get the last message per group
        // Uses id matching with ORDER BY LIMIT 1 for deterministic results on timestamp ties
        let mut qb: sqlx::QueryBuilder<sqlx::Sqlite> = sqlx::QueryBuilder::new(
            "SELECT message_id, mls_group_id, author, content, created_at, \
             json_array_length(media_attachments) as media_count \
             FROM aggregated_messages am1 \
             WHERE kind = 9 AND mls_group_id IN (",
        );
        let mut sep = qb.separated(", ");
        for id in &group_id_bytes {
            sep.push_bind(id);
        }
        sep.push_unseparated(
            ") AND deletion_event_id IS NULL \
             AND id = ( \
                 SELECT id FROM aggregated_messages am2 \
                 WHERE am2.mls_group_id = am1.mls_group_id \
                   AND am2.kind = 9 \
                   AND am2.deletion_event_id IS NULL \
                 ORDER BY created_at DESC, id DESC \
                 LIMIT 1 \
             )",
        );

        let rows = qb.build().fetch_all(&database.pool).await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            let message_id_hex: String = row.try_get("message_id")?;
            let message_id =
                EventId::from_hex(&message_id_hex).map_err(|e| sqlx::Error::ColumnDecode {
                    index: "message_id".to_string(),
                    source: Box::new(e),
                })?;
            let mls_group_id_bytes: Vec<u8> = row.try_get("mls_group_id")?;
            let mls_group_id = GroupId::from_slice(&mls_group_id_bytes);

            let author_hex: String = row.try_get("author")?;
            let author =
                PublicKey::from_hex(&author_hex).map_err(|e| sqlx::Error::ColumnDecode {
                    index: "author".to_string(),
                    source: Box::new(e),
                })?;

            let content: String = row.try_get("content")?;
            let created_at = parse_timestamp(&row, "created_at")?;
            let media_count: i64 = row.try_get("media_count")?;

            let summary = ChatMessageSummary {
                message_id,
                mls_group_id,
                author,
                author_display_name: None,
                content,
                created_at,
                media_attachment_count: media_count as usize,
            };

            results.push(summary);
        }

        Ok(results)
    }

    /// Convert database row to ChatMessage
    fn row_to_chat_message(row: AggregatedMessageRow) -> Result<ChatMessage> {
        // Convert DateTime<Utc> to Timestamp (seconds)
        let created_at = Timestamp::from(row.created_at.timestamp() as u64);

        Ok(ChatMessage {
            id: row.message_id.to_string(),
            author: row.author,
            content: row.content,
            created_at,
            tags: row.tags,
            is_reply: row.reply_to_id.is_some(),
            reply_to_id: row.reply_to_id.map(|id| id.to_string()),
            is_deleted: row.deletion_event_id.is_some(),
            content_tokens: row.content_tokens,
            reactions: row.reactions,
            kind: row.kind.as_u16(),
            media_attachments: row.media_attachments,
            delivery_status: row.delivery_status,
        })
    }

    /// Create a minimal test message with specific timestamp.
    /// This is only used for testing the update_last_read timestamp comparison logic.
    #[cfg(test)]
    pub(crate) async fn create_for_test(
        message_id: EventId,
        group_id: GroupId,
        author: PublicKey,
        created_at: DateTime<Utc>,
        database: &Database,
    ) -> Result<()> {
        let empty_tokens = Vec::<SerializableToken>::new();
        let empty_reactions = ReactionSummary::default();
        let empty_media = Vec::<MediaFile>::new();

        sqlx::query(
            "INSERT INTO aggregated_messages
             (message_id, mls_group_id, author, created_at, kind, content, tags,
              content_tokens, reactions, media_attachments)
             VALUES (?, ?, ?, ?, 9, '', '[]', ?, ?, ?)",
        )
        .bind(message_id.to_hex())
        .bind(group_id.as_slice())
        .bind(author.to_hex())
        .bind(created_at.timestamp_millis())
        .bind(serde_json::to_string(&empty_tokens)?)
        .bind(serde_json::to_string(&empty_reactions)?)
        .bind(serde_json::to_string(&empty_media)?)
        .execute(&database.pool)
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::Keys;

    use super::*;
    use crate::whitenoise::group_information::{GroupInformation, GroupType};
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    async fn setup_group(group_id: &GroupId, database: &Database) {
        // Create group_information record (required for foreign key constraint)
        GroupInformation::find_or_create_by_mls_group_id(
            group_id,
            Some(GroupType::Group),
            database,
        )
        .await
        .unwrap();
    }

    fn create_test_chat_message(seed: u8, author: PublicKey) -> ChatMessage {
        // Create a valid 64-character hex string by repeating a pattern
        let id = format!("{:0>64}", format!("{:x}", seed));

        ChatMessage {
            id,
            author,
            content: "Test message".to_string(),
            created_at: Timestamp::now(),
            tags: Tags::new(),
            is_reply: false,
            reply_to_id: None,
            is_deleted: false,
            content_tokens: vec![],
            reactions: ReactionSummary::default(),
            kind: 9,
            media_attachments: vec![],
            delivery_status: None,
        }
    }

    #[tokio::test]
    async fn test_count_by_group_empty() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[1; 32]);

        let count = AggregatedMessage::count_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_get_all_event_ids_empty() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[1; 32]);

        let ids = AggregatedMessage::get_all_event_ids_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn test_find_messages_by_group_empty() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[1; 32]);

        let messages = AggregatedMessage::find_messages_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert!(messages.is_empty());
    }

    #[tokio::test]
    async fn test_insert_message() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[1; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        let message = create_test_chat_message(1, author);

        // Insert message
        let result =
            AggregatedMessage::insert_message(&message, &group_id, &whitenoise.database).await;
        assert!(result.is_ok());

        // Verify it was inserted
        let count = AggregatedMessage::count_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(count, 1);

        // Verify we can retrieve it
        let messages = AggregatedMessage::find_messages_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, message.id);
        assert_eq!(messages[0].content, message.content);
    }

    #[tokio::test]
    async fn test_insert_multiple_messages() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[2; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();

        // Insert multiple messages
        let mut message_ids = vec![];
        for i in 1..=3 {
            let message = create_test_chat_message(i, author);
            message_ids.push(message.id.clone());
            AggregatedMessage::insert_message(&message, &group_id, &whitenoise.database)
                .await
                .unwrap();
        }

        // Verify count
        let count = AggregatedMessage::count_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(count, 3);

        // Verify we can retrieve all messages
        let messages = AggregatedMessage::find_messages_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(messages.len(), 3);

        // Verify event IDs
        let event_ids =
            AggregatedMessage::get_all_event_ids_by_group(&group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(event_ids.len(), 3);
        for id in &message_ids {
            assert!(event_ids.contains(id));
        }
    }

    #[tokio::test]
    async fn test_mark_deleted_does_not_decrease_count() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[3; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();

        // Insert a message
        let message = create_test_chat_message(10, author);
        AggregatedMessage::insert_message(&message, &group_id, &whitenoise.database)
            .await
            .unwrap();

        let count_before = AggregatedMessage::count_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(count_before, 1);

        // Mark as deleted - need a valid 64-char hex ID
        let deletion_event_id = format!("{:0>64}", "abc123");
        AggregatedMessage::mark_deleted(
            &message.id,
            &group_id,
            &deletion_event_id,
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Count should remain the same - mark_deleted doesn't remove the row
        let count_after = AggregatedMessage::count_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(count_after, 1);

        // But the message should have deletion_event_id set
        let messages = AggregatedMessage::find_messages_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].is_deleted);
    }

    #[tokio::test]
    async fn test_delete_by_group_removes_all_events() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[4; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();

        // Insert multiple messages
        let message1 = create_test_chat_message(20, author);
        AggregatedMessage::insert_message(&message1, &group_id, &whitenoise.database)
            .await
            .unwrap();

        let message2 = create_test_chat_message(21, author);
        AggregatedMessage::insert_message(&message2, &group_id, &whitenoise.database)
            .await
            .unwrap();

        let message3 = create_test_chat_message(22, author);
        AggregatedMessage::insert_message(&message3, &group_id, &whitenoise.database)
            .await
            .unwrap();

        // Verify count before deletion
        let count_before = AggregatedMessage::count_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(count_before, 3);

        // Delete all events for the group
        AggregatedMessage::delete_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();

        // Count should now be zero
        let count_after = AggregatedMessage::count_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(count_after, 0);

        // No messages should be found
        let messages = AggregatedMessage::find_messages_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert!(messages.is_empty());

        // No event IDs should be found
        let event_ids =
            AggregatedMessage::get_all_event_ids_by_group(&group_id, &whitenoise.database)
                .await
                .unwrap();
        assert!(event_ids.is_empty());
    }

    #[tokio::test]
    async fn test_delete_by_group_is_group_specific() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id_1 = GroupId::from_slice(&[5; 32]);
        let group_id_2 = GroupId::from_slice(&[6; 32]);
        setup_group(&group_id_1, &whitenoise.database).await;
        setup_group(&group_id_2, &whitenoise.database).await;

        let author = Keys::generate().public_key();

        // Insert message in group 1
        let message1 = create_test_chat_message(30, author);
        AggregatedMessage::insert_message(&message1, &group_id_1, &whitenoise.database)
            .await
            .unwrap();

        // Insert message in group 2
        let message2 = create_test_chat_message(31, author);
        AggregatedMessage::insert_message(&message2, &group_id_2, &whitenoise.database)
            .await
            .unwrap();

        // Delete group 1
        AggregatedMessage::delete_by_group(&group_id_1, &whitenoise.database)
            .await
            .unwrap();

        // Group 1 should be empty
        let count_1 = AggregatedMessage::count_by_group(&group_id_1, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(count_1, 0);

        // Group 2 should still have its message
        let count_2 = AggregatedMessage::count_by_group(&group_id_2, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(count_2, 1);
    }

    #[tokio::test]
    async fn test_update_reactions() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[7; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();

        // Insert a message with empty reactions
        let message = create_test_chat_message(40, author);
        AggregatedMessage::insert_message(&message, &group_id, &whitenoise.database)
            .await
            .unwrap();

        // Update with reactions
        let mut reactions = ReactionSummary::default();
        reactions.by_emoji.insert(
            "👍".to_string(),
            crate::whitenoise::message_aggregator::EmojiReaction {
                emoji: "👍".to_string(),
                count: 2,
                users: vec![author, Keys::generate().public_key()],
            },
        );

        AggregatedMessage::update_reactions(
            &message.id,
            &group_id,
            &reactions,
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Verify reactions were updated
        let messages = AggregatedMessage::find_messages_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].reactions.by_emoji.len(), 1);
        assert!(messages[0].reactions.by_emoji.contains_key("👍"));
    }

    #[tokio::test]
    async fn test_find_last_by_group_ids_empty_input() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let result = AggregatedMessage::find_last_by_group_ids(&[], &whitenoise.database)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_find_last_by_group_ids_comprehensive() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Setup groups with different scenarios
        let group_with_media = GroupId::from_slice(&[50; 32]);
        let group_no_media = GroupId::from_slice(&[51; 32]);
        let group_with_deletion = GroupId::from_slice(&[52; 32]);
        let group_empty = GroupId::from_slice(&[53; 32]);
        let group_multiple_messages = GroupId::from_slice(&[54; 32]);

        for group_id in [
            &group_with_media,
            &group_no_media,
            &group_with_deletion,
            &group_empty,
            &group_multiple_messages,
        ] {
            setup_group(group_id, &whitenoise.database).await;
        }

        let author = Keys::generate().public_key();

        // Group 1: Message with 2 media attachments
        let mut msg_with_media = create_test_chat_message(50, author);
        msg_with_media.content = "Message with media".to_string();
        msg_with_media.media_attachments = vec![
            MediaFile {
                id: Some(1),
                mls_group_id: group_with_media.clone(),
                account_pubkey: author,
                file_path: std::path::PathBuf::from("/path/to/file1"),
                original_file_hash: Some(vec![1; 32]),
                encrypted_file_hash: vec![2; 32],
                mime_type: "image/png".to_string(),
                media_type: "image".to_string(),
                blossom_url: None,
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
                created_at: chrono::Utc::now(),
            },
            MediaFile {
                id: Some(2),
                mls_group_id: group_with_media.clone(),
                account_pubkey: author,
                file_path: std::path::PathBuf::from("/path/to/file2"),
                original_file_hash: Some(vec![3; 32]),
                encrypted_file_hash: vec![4; 32],
                mime_type: "image/jpeg".to_string(),
                media_type: "image".to_string(),
                blossom_url: None,
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
                created_at: chrono::Utc::now(),
            },
        ];
        AggregatedMessage::insert_message(&msg_with_media, &group_with_media, &whitenoise.database)
            .await
            .unwrap();

        // Group 2: Message without media
        let mut msg_no_media = create_test_chat_message(51, author);
        msg_no_media.content = "Message without media".to_string();
        AggregatedMessage::insert_message(&msg_no_media, &group_no_media, &whitenoise.database)
            .await
            .unwrap();

        // Group 3: Has deleted newest message, should return older one
        let mut msg_older = create_test_chat_message(52, author);
        msg_older.content = "Older non-deleted".to_string();
        msg_older.created_at = Timestamp::from(1000);
        AggregatedMessage::insert_message(&msg_older, &group_with_deletion, &whitenoise.database)
            .await
            .unwrap();

        let mut msg_deleted = create_test_chat_message(53, author);
        msg_deleted.content = "Deleted message".to_string();
        msg_deleted.created_at = Timestamp::from(2000);
        AggregatedMessage::insert_message(&msg_deleted, &group_with_deletion, &whitenoise.database)
            .await
            .unwrap();
        AggregatedMessage::mark_deleted(
            &msg_deleted.id,
            &group_with_deletion,
            &format!("{:0>64}", "del"),
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Group 4: Empty (no messages) - already set up, no messages added

        // Group 5: Multiple messages, should return the last one
        let mut msg_first = create_test_chat_message(54, author);
        msg_first.content = "First message".to_string();
        msg_first.created_at = Timestamp::from(1000);
        AggregatedMessage::insert_message(
            &msg_first,
            &group_multiple_messages,
            &whitenoise.database,
        )
        .await
        .unwrap();

        let mut msg_last = create_test_chat_message(55, author);
        msg_last.content = "Last message".to_string();
        msg_last.created_at = Timestamp::from(2000);
        AggregatedMessage::insert_message(
            &msg_last,
            &group_multiple_messages,
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Query all groups
        let result = AggregatedMessage::find_last_by_group_ids(
            &[
                group_with_media.clone(),
                group_no_media.clone(),
                group_with_deletion.clone(),
                group_empty.clone(),
                group_multiple_messages.clone(),
            ],
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Should return 4 results (empty group excluded)
        assert_eq!(result.len(), 4);

        // Convert to HashMap for easier assertions
        let map: std::collections::HashMap<_, _> = result
            .into_iter()
            .map(|s| (s.mls_group_id.clone(), s))
            .collect();

        // Verify each group's result
        assert_eq!(map[&group_with_media].content, "Message with media");
        assert_eq!(map[&group_with_media].media_attachment_count, 2);

        assert_eq!(map[&group_no_media].content, "Message without media");
        assert_eq!(map[&group_no_media].media_attachment_count, 0);

        assert_eq!(map[&group_with_deletion].content, "Older non-deleted");

        assert_eq!(map[&group_multiple_messages].content, "Last message");

        // Empty group should not be in results
        assert!(!map.contains_key(&group_empty));

        // All should have author_display_name as None
        for summary in map.values() {
            assert_eq!(summary.author_display_name, None);
            assert_eq!(summary.author, author);
        }
    }

    #[tokio::test]
    async fn test_find_by_message_id_returns_message() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[70; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        let message = create_test_chat_message(70, author);
        AggregatedMessage::insert_message(&message, &group_id, &whitenoise.database)
            .await
            .unwrap();

        let event_id = EventId::from_hex(&message.id).unwrap();
        let found = AggregatedMessage::find_by_message_id(&event_id, &whitenoise.database)
            .await
            .unwrap();

        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.event_id, event_id);
        assert_eq!(found.mls_group_id, group_id);
    }

    #[tokio::test]
    async fn test_find_by_message_id_returns_none_for_nonexistent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let fake_id = EventId::all_zeros();

        let found = AggregatedMessage::find_by_message_id(&fake_id, &whitenoise.database)
            .await
            .unwrap();

        assert!(found.is_none());
    }

    #[tokio::test]
    async fn test_count_unread_for_group_no_read_marker_returns_all() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[80; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        for i in 1..=3 {
            let msg = create_test_chat_message(80 + i, author);
            AggregatedMessage::insert_message(&msg, &group_id, &whitenoise.database)
                .await
                .unwrap();
        }

        let count =
            AggregatedMessage::count_unread_for_group(&group_id, None, &whitenoise.database)
                .await
                .unwrap();

        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_count_unread_for_group_with_read_marker() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[90; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        let mut messages = Vec::new();
        for i in 1..=5u8 {
            let mut msg = create_test_chat_message(90 + i, author);
            msg.created_at = Timestamp::from(i as u64 * 1000);
            AggregatedMessage::insert_message(&msg, &group_id, &whitenoise.database)
                .await
                .unwrap();
            messages.push(msg);
        }

        // Read marker at message 2 -> 3 unread (messages 3, 4, 5)
        let read_marker_id = EventId::from_hex(&messages[1].id).unwrap();
        let count = AggregatedMessage::count_unread_for_group(
            &group_id,
            Some(&read_marker_id),
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_count_unread_for_group_excludes_deleted() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[100; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        let msg1 = create_test_chat_message(100, author);
        let msg2 = create_test_chat_message(101, author);
        AggregatedMessage::insert_message(&msg1, &group_id, &whitenoise.database)
            .await
            .unwrap();
        AggregatedMessage::insert_message(&msg2, &group_id, &whitenoise.database)
            .await
            .unwrap();

        // Delete msg2
        AggregatedMessage::mark_deleted(
            &msg2.id,
            &group_id,
            &format!("{:0>64}", "del"),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let count =
            AggregatedMessage::count_unread_for_group(&group_id, None, &whitenoise.database)
                .await
                .unwrap();

        assert_eq!(count, 1); // Only msg1 counted
    }

    #[tokio::test]
    async fn test_count_unread_for_groups_empty_input() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let result = AggregatedMessage::count_unread_for_groups(&[], &whitenoise.database)
            .await
            .unwrap();

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_count_unread_for_groups_no_markers() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let group1 = GroupId::from_slice(&[110; 32]);
        let group2 = GroupId::from_slice(&[111; 32]);
        let group3 = GroupId::from_slice(&[112; 32]); // Empty group

        for group_id in [&group1, &group2, &group3] {
            setup_group(group_id, &whitenoise.database).await;
        }

        let author = Keys::generate().public_key();

        // Group 1: 3 messages
        for i in 1..=3 {
            let msg = create_test_chat_message(110 + i, author);
            AggregatedMessage::insert_message(&msg, &group1, &whitenoise.database)
                .await
                .unwrap();
        }

        // Group 2: 5 messages
        for i in 1..=5 {
            let msg = create_test_chat_message(120 + i, author);
            AggregatedMessage::insert_message(&msg, &group2, &whitenoise.database)
                .await
                .unwrap();
        }

        // Group 3: no messages

        let input = vec![
            (group1.clone(), None),
            (group2.clone(), None),
            (group3.clone(), None),
        ];

        let result = AggregatedMessage::count_unread_for_groups(&input, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(result[&group1], 3);
        assert_eq!(result[&group2], 5);
        assert_eq!(result[&group3], 0);
    }

    #[tokio::test]
    async fn test_count_unread_for_groups_with_markers() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let group1 = GroupId::from_slice(&[130; 32]);
        let group2 = GroupId::from_slice(&[131; 32]);

        for group_id in [&group1, &group2] {
            setup_group(group_id, &whitenoise.database).await;
        }

        let author = Keys::generate().public_key();

        // Group 1: 5 messages, read marker at message 2
        let mut group1_messages = Vec::new();
        for i in 1..=5u8 {
            let mut msg = create_test_chat_message(130 + i, author);
            msg.created_at = Timestamp::from(i as u64 * 1000);
            AggregatedMessage::insert_message(&msg, &group1, &whitenoise.database)
                .await
                .unwrap();
            group1_messages.push(msg);
        }
        let marker1 = EventId::from_hex(&group1_messages[1].id).unwrap();

        // Group 2: 4 messages, read marker at message 3
        let mut group2_messages = Vec::new();
        for i in 1..=4u8 {
            let mut msg = create_test_chat_message(140 + i, author);
            msg.created_at = Timestamp::from(i as u64 * 1000);
            AggregatedMessage::insert_message(&msg, &group2, &whitenoise.database)
                .await
                .unwrap();
            group2_messages.push(msg);
        }
        let marker2 = EventId::from_hex(&group2_messages[2].id).unwrap();

        let input = vec![
            (group1.clone(), Some(marker1)),
            (group2.clone(), Some(marker2)),
        ];

        let result = AggregatedMessage::count_unread_for_groups(&input, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[&group1], 3); // Messages 3, 4, 5 unread
        assert_eq!(result[&group2], 1); // Message 4 unread
    }

    #[tokio::test]
    async fn test_count_unread_for_groups_mixed_markers() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let group_with_marker = GroupId::from_slice(&[150; 32]);
        let group_without_marker = GroupId::from_slice(&[151; 32]);
        let group_empty = GroupId::from_slice(&[152; 32]);

        for group_id in [&group_with_marker, &group_without_marker, &group_empty] {
            setup_group(group_id, &whitenoise.database).await;
        }

        let author = Keys::generate().public_key();

        // Group with marker: 4 messages, read marker at message 2
        let mut messages = Vec::new();
        for i in 1..=4u8 {
            let mut msg = create_test_chat_message(150 + i, author);
            msg.created_at = Timestamp::from(i as u64 * 1000);
            AggregatedMessage::insert_message(&msg, &group_with_marker, &whitenoise.database)
                .await
                .unwrap();
            messages.push(msg);
        }
        let marker = EventId::from_hex(&messages[1].id).unwrap();

        // Group without marker: 3 messages
        for i in 1..=3 {
            let msg = create_test_chat_message(160 + i, author);
            AggregatedMessage::insert_message(&msg, &group_without_marker, &whitenoise.database)
                .await
                .unwrap();
        }

        let input = vec![
            (group_with_marker.clone(), Some(marker)),
            (group_without_marker.clone(), None),
            (group_empty.clone(), None),
        ];

        let result = AggregatedMessage::count_unread_for_groups(&input, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(result[&group_with_marker], 2); // Messages 3, 4 unread
        assert_eq!(result[&group_without_marker], 3); // All unread
        assert_eq!(result[&group_empty], 0);
    }

    #[tokio::test]
    async fn test_update_delivery_status_and_find_by_id() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[200; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();

        // Insert a message with Sending status
        let mut message = create_test_chat_message(200, author);
        message.delivery_status = Some(DeliveryStatus::Sending);
        AggregatedMessage::insert_message(&message, &group_id, &whitenoise.database)
            .await
            .unwrap();

        // Verify initial Sending status via find_by_id
        let found = AggregatedMessage::find_by_id(&message.id, &group_id, &whitenoise.database)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.delivery_status, Some(DeliveryStatus::Sending));

        // Update to Sent(3) — returned message should have the updated status
        let updated = AggregatedMessage::update_delivery_status(
            &message.id,
            &group_id,
            &DeliveryStatus::Sent(3),
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(updated.delivery_status, Some(DeliveryStatus::Sent(3)));

        // Verify via find_by_id as well
        let found = AggregatedMessage::find_by_id(&message.id, &group_id, &whitenoise.database)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.delivery_status, Some(DeliveryStatus::Sent(3)));

        // Update to Failed
        let updated = AggregatedMessage::update_delivery_status(
            &message.id,
            &group_id,
            &DeliveryStatus::Failed("timeout".to_string()),
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(
            updated.delivery_status,
            Some(DeliveryStatus::Failed("timeout".to_string()))
        );
    }

    #[tokio::test]
    async fn test_find_by_id_returns_none_for_nonexistent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[201; 32]);

        let found = AggregatedMessage::find_by_id("nonexistent", &group_id, &whitenoise.database)
            .await
            .unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn test_insert_message_with_no_delivery_status() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[202; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        let message = create_test_chat_message(202, author);
        // delivery_status is None (incoming message)
        assert!(message.delivery_status.is_none());

        AggregatedMessage::insert_message(&message, &group_id, &whitenoise.database)
            .await
            .unwrap();

        let found = AggregatedMessage::find_by_id(&message.id, &group_id, &whitenoise.database)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.delivery_status, None);
    }

    #[tokio::test]
    async fn test_find_messages_by_group_preserves_delivery_status() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[203; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();

        // Insert incoming message (no delivery status)
        let msg_incoming = create_test_chat_message(203, author);
        AggregatedMessage::insert_message(&msg_incoming, &group_id, &whitenoise.database)
            .await
            .unwrap();

        // Insert outgoing message with Sent status
        let mut msg_outgoing = create_test_chat_message(204, author);
        msg_outgoing.delivery_status = Some(DeliveryStatus::Sent(2));
        AggregatedMessage::insert_message(&msg_outgoing, &group_id, &whitenoise.database)
            .await
            .unwrap();

        let messages = AggregatedMessage::find_messages_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(messages.len(), 2);

        let statuses: Vec<_> = messages.iter().map(|m| &m.delivery_status).collect();
        assert!(statuses.contains(&&None));
        assert!(statuses.contains(&&Some(DeliveryStatus::Sent(2))));
    }

    #[tokio::test]
    async fn test_update_delivery_status_returns_error_for_nonexistent_message() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[205; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        // Try to update delivery status for a message that doesn't exist
        let result = AggregatedMessage::update_delivery_status(
            &format!("{:0>64}", "ff"),
            &group_id,
            &DeliveryStatus::Sent(1),
            &whitenoise.database,
        )
        .await;

        assert!(
            result.is_err(),
            "Should return error for nonexistent message"
        );
        let err = result.unwrap_err();
        assert!(
            matches!(err, DatabaseError::Sqlx(sqlx::Error::RowNotFound)),
            "Expected RowNotFound error, got: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn test_find_messages_by_group_excludes_retried() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[206; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();

        // Insert a normal message
        let msg_normal = create_test_chat_message(206, author);
        AggregatedMessage::insert_message(&msg_normal, &group_id, &whitenoise.database)
            .await
            .unwrap();

        // Insert a message with Retried status (simulating a retried failed message)
        let mut msg_retried = create_test_chat_message(207, author);
        msg_retried.delivery_status = Some(DeliveryStatus::Retried);
        AggregatedMessage::insert_message(&msg_retried, &group_id, &whitenoise.database)
            .await
            .unwrap();

        // Insert a message with Failed status (should still be visible)
        let mut msg_failed = create_test_chat_message(208, author);
        msg_failed.delivery_status = Some(DeliveryStatus::Failed("error".to_string()));
        AggregatedMessage::insert_message(&msg_failed, &group_id, &whitenoise.database)
            .await
            .unwrap();

        let messages = AggregatedMessage::find_messages_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();

        // Only normal and failed messages should appear, not retried
        assert_eq!(messages.len(), 2);
        let ids: Vec<&str> = messages.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&msg_normal.id.as_str()));
        assert!(ids.contains(&msg_failed.id.as_str()));
        assert!(!ids.contains(&msg_retried.id.as_str()));
    }

    #[tokio::test]
    async fn test_count_unread_for_groups_excludes_deleted() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let group_id = GroupId::from_slice(&[170; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        let msg1 = create_test_chat_message(170, author);
        let msg2 = create_test_chat_message(171, author);
        let msg3 = create_test_chat_message(172, author);

        for msg in [&msg1, &msg2, &msg3] {
            AggregatedMessage::insert_message(msg, &group_id, &whitenoise.database)
                .await
                .unwrap();
        }

        // Delete msg2
        AggregatedMessage::mark_deleted(
            &msg2.id,
            &group_id,
            &format!("{:0>64}", "del"),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let input = vec![(group_id.clone(), None)];
        let result = AggregatedMessage::count_unread_for_groups(&input, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(result[&group_id], 2); // msg1 and msg3, excluding deleted msg2
    }

    #[tokio::test]
    async fn test_insert_delivery_status_and_has_delivery_status() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[180; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        let msg = create_test_chat_message(180, author);
        AggregatedMessage::insert_message(&msg, &group_id, &whitenoise.database)
            .await
            .unwrap();

        // Before inserting, has_delivery_status should return false
        let has = AggregatedMessage::has_delivery_status(&msg.id, &group_id, &whitenoise.database)
            .await
            .unwrap();
        assert!(!has, "No delivery status should exist yet");

        // Insert delivery status
        AggregatedMessage::insert_delivery_status(
            &msg.id,
            &group_id,
            &DeliveryStatus::Sending,
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Now has_delivery_status should return true
        let has = AggregatedMessage::has_delivery_status(&msg.id, &group_id, &whitenoise.database)
            .await
            .unwrap();
        assert!(has, "Delivery status should exist after insert");

        // Verify it shows up in find_by_id
        let found = AggregatedMessage::find_by_id(&msg.id, &group_id, &whitenoise.database)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.delivery_status, Some(DeliveryStatus::Sending));
    }

    #[tokio::test]
    async fn test_insert_delivery_status_upsert() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[181; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        let msg = create_test_chat_message(181, author);
        AggregatedMessage::insert_message(&msg, &group_id, &whitenoise.database)
            .await
            .unwrap();

        // Insert Sending
        AggregatedMessage::insert_delivery_status(
            &msg.id,
            &group_id,
            &DeliveryStatus::Sending,
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Upsert to Sent — ON CONFLICT should update
        AggregatedMessage::insert_delivery_status(
            &msg.id,
            &group_id,
            &DeliveryStatus::Sent(2),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let found = AggregatedMessage::find_by_id(&msg.id, &group_id, &whitenoise.database)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.delivery_status, Some(DeliveryStatus::Sent(2)));
    }

    #[tokio::test]
    async fn test_unmark_deleted_reverses_deletion() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[182; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        let msg1 = create_test_chat_message(182, author);
        let msg2 = create_test_chat_message(183, author);
        AggregatedMessage::insert_message(&msg1, &group_id, &whitenoise.database)
            .await
            .unwrap();
        AggregatedMessage::insert_message(&msg2, &group_id, &whitenoise.database)
            .await
            .unwrap();

        let del_id = format!("{:0>64x}", 0xde1182u64);

        // Mark both as deleted by the same deletion event
        AggregatedMessage::mark_deleted(&msg1.id, &group_id, &del_id, &whitenoise.database)
            .await
            .unwrap();
        AggregatedMessage::mark_deleted(&msg2.id, &group_id, &del_id, &whitenoise.database)
            .await
            .unwrap();

        // Both should have is_deleted=true
        let messages = AggregatedMessage::find_messages_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert!(
            messages.iter().all(|m| m.is_deleted),
            "All messages should be marked as deleted"
        );

        // Unmark — both should revert to not deleted
        AggregatedMessage::unmark_deleted(&del_id, &group_id, &whitenoise.database)
            .await
            .unwrap();

        let messages = AggregatedMessage::find_messages_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(messages.len(), 2, "Both messages should still be present");
        assert!(
            messages.iter().all(|m| !m.is_deleted),
            "All messages should be un-deleted after unmark"
        );
    }

    #[tokio::test]
    async fn test_unmark_deleted_only_affects_matching_deletion() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[184; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        let msg1 = create_test_chat_message(184, author);
        let msg2 = create_test_chat_message(185, author);
        AggregatedMessage::insert_message(&msg1, &group_id, &whitenoise.database)
            .await
            .unwrap();
        AggregatedMessage::insert_message(&msg2, &group_id, &whitenoise.database)
            .await
            .unwrap();

        let del_a = format!("{:0>64x}", 0xde1au64);
        let del_b = format!("{:0>64x}", 0xde1bu64);

        AggregatedMessage::mark_deleted(&msg1.id, &group_id, &del_a, &whitenoise.database)
            .await
            .unwrap();
        AggregatedMessage::mark_deleted(&msg2.id, &group_id, &del_b, &whitenoise.database)
            .await
            .unwrap();

        // Unmark only del_a — msg1 should revert to not-deleted, msg2 stays deleted
        AggregatedMessage::unmark_deleted(&del_a, &group_id, &whitenoise.database)
            .await
            .unwrap();

        let messages = AggregatedMessage::find_messages_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        let not_deleted: Vec<_> = messages.iter().filter(|m| !m.is_deleted).collect();
        assert_eq!(not_deleted.len(), 1, "Only msg1 should be un-deleted");
        assert_eq!(not_deleted[0].id, msg1.id);
    }

    #[tokio::test]
    async fn test_find_orphaned_reactions_excludes_deleted() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[186; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        // Use valid hex IDs (find_orphaned_reactions parses message_id as EventId)
        let parent_id = format!("{:0>64x}", 0xba186u64);
        let reaction_id = format!("{:0>64x}", 0xea186u64);

        // Insert a reaction targeting the parent, using direct SQL since
        // insert_reaction needs a Message struct from MDK
        let tags_json = serde_json::to_string(&vec![vec!["e", &parent_id]]).unwrap();
        let empty_tokens = serde_json::to_string(&Vec::<String>::new()).unwrap();
        let empty_reactions = serde_json::to_string(&ReactionSummary::default()).unwrap();
        let empty_media = serde_json::to_string(&Vec::<String>::new()).unwrap();

        sqlx::query(
            "INSERT INTO aggregated_messages
             (message_id, mls_group_id, author, created_at, kind, content, tags,
              content_tokens, reactions, media_attachments)
             VALUES (?, ?, ?, ?, 7, '+', ?, ?, ?, ?)",
        )
        .bind(&reaction_id)
        .bind(group_id.as_slice())
        .bind(author.to_hex())
        .bind(1000i64)
        .bind(&tags_json)
        .bind(&empty_tokens)
        .bind(&empty_reactions)
        .bind(&empty_media)
        .execute(&whitenoise.database.pool)
        .await
        .unwrap();

        // The reaction should appear as orphaned
        let orphans =
            AggregatedMessage::find_orphaned_reactions(&parent_id, &group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(orphans.len(), 1, "Non-deleted reaction should be found");

        // Now mark the reaction as deleted
        let del_id = format!("{:0>64x}", 0xde1186u64);
        AggregatedMessage::mark_deleted(&reaction_id, &group_id, &del_id, &whitenoise.database)
            .await
            .unwrap();

        // Deleted reaction should NOT appear as orphaned
        let orphans =
            AggregatedMessage::find_orphaned_reactions(&parent_id, &group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(orphans.len(), 0, "Deleted reaction should be excluded");
    }

    #[tokio::test]
    async fn test_has_delivery_status_returns_false_for_nonexistent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[188; 32]);

        let has =
            AggregatedMessage::has_delivery_status("nonexistent", &group_id, &whitenoise.database)
                .await
                .unwrap();
        assert!(!has);
    }

    // ── find_messages_by_group_paginated ─────────────────────────────────────

    /// Helper: insert a chat message with an explicit Unix-seconds timestamp.
    async fn insert_message_at(
        seed: u8,
        author: PublicKey,
        unix_secs: u64,
        group_id: &GroupId,
        database: &Database,
    ) -> ChatMessage {
        let id = format!("{:0>64}", format!("{:x}", seed));
        let msg = ChatMessage {
            id,
            author,
            content: format!("message {seed}"),
            created_at: Timestamp::from(unix_secs),
            tags: Tags::new(),
            is_reply: false,
            reply_to_id: None,
            is_deleted: false,
            content_tokens: vec![],
            reactions: ReactionSummary::default(),
            kind: 9,
            media_attachments: vec![],
            delivery_status: None,
        };
        AggregatedMessage::insert_message(&msg, group_id, database)
            .await
            .unwrap();
        msg
    }

    #[tokio::test]
    async fn test_paginated_empty_group_returns_empty() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[200; 32]);

        let messages = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(messages.is_empty());
    }

    #[tokio::test]
    async fn test_paginated_default_returns_up_to_50_newest() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[201; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        let base_ts: u64 = 1_700_000_000;

        // Insert 60 messages with ascending timestamps
        for i in 1u8..=60 {
            insert_message_at(
                i,
                author,
                base_ts + u64::from(i),
                &group_id,
                &whitenoise.database,
            )
            .await;
        }

        // Default (None, None) should return the 50 newest
        let messages = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(messages.len(), 50, "should return exactly 50 messages");

        // Verify oldest-first ordering
        for w in messages.windows(2) {
            assert!(
                w[0].created_at <= w[1].created_at,
                "messages must be oldest-first"
            );
        }

        // The 50 newest are seeds 11–60 (timestamps base+11 … base+60)
        assert_eq!(
            messages[0].created_at,
            Timestamp::from(base_ts + 11),
            "first returned message should be the 11th oldest (50th from the end)"
        );
        assert_eq!(
            messages[49].created_at,
            Timestamp::from(base_ts + 60),
            "last returned message should be the newest"
        );
    }

    #[tokio::test]
    async fn test_paginated_before_cursor_excludes_on_or_after() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[202; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        let base_ts: u64 = 1_700_100_000;

        // Insert 5 messages: ts+1, ts+2, ts+3, ts+4, ts+5
        for i in 1u8..=5 {
            insert_message_at(
                i,
                author,
                base_ts + u64::from(i),
                &group_id,
                &whitenoise.database,
            )
            .await;
        }

        // Cursor at ts+3 (seed 3): should return only messages with created_at < ts+3 → ts+1, ts+2.
        // Both halves of the compound cursor are required.
        let cursor_ts = Timestamp::from(base_ts + 3);
        let cursor_id = format!("{:0>64}", format!("{:x}", 3u8));
        let messages = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            Some(cursor_ts),
            Some(cursor_id.as_str()),
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            messages.len(),
            2,
            "should return 2 messages before the cursor"
        );
        assert_eq!(messages[0].created_at, Timestamp::from(base_ts + 1));
        assert_eq!(messages[1].created_at, Timestamp::from(base_ts + 2));
    }

    #[tokio::test]
    async fn test_paginated_limit_is_respected() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[203; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        let base_ts: u64 = 1_700_200_000;

        for i in 1u8..=10 {
            insert_message_at(
                i,
                author,
                base_ts + u64::from(i),
                &group_id,
                &whitenoise.database,
            )
            .await;
        }

        let messages = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            None,
            None,
            Some(3),
        )
        .await
        .unwrap();

        assert_eq!(
            messages.len(),
            3,
            "should return exactly the requested limit"
        );

        // The 3 newest: seeds 8, 9, 10
        assert_eq!(messages[0].created_at, Timestamp::from(base_ts + 8));
        assert_eq!(messages[2].created_at, Timestamp::from(base_ts + 10));
    }

    #[tokio::test]
    async fn test_paginated_limit_capped_at_200() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[204; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        let base_ts: u64 = 1_700_300_000;

        // Insert 210 messages (more than the cap)
        for i in 0u8..=209 {
            // Use two bytes for the seed to stay within u8 — spread across different IDs
            let id_seed = i;
            insert_message_at(
                id_seed,
                author,
                base_ts + u64::from(i),
                &group_id,
                &whitenoise.database,
            )
            .await;
        }

        // Requesting u32::MAX should be capped to 200
        let messages = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            None,
            None,
            Some(u32::MAX),
        )
        .await
        .unwrap();

        assert_eq!(messages.len(), 200, "limit should be capped at 200");
    }

    #[tokio::test]
    async fn test_paginated_pages_are_contiguous_without_overlap_or_gap() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[205; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();
        let base_ts: u64 = 1_700_400_000;

        // Insert 10 messages with distinct timestamps
        for i in 1u8..=10 {
            insert_message_at(
                i,
                author,
                base_ts + u64::from(i),
                &group_id,
                &whitenoise.database,
            )
            .await;
        }

        // Page 1: newest 5 (seeds 6–10)
        let page1 = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            None,
            None,
            Some(5),
        )
        .await
        .unwrap();
        assert_eq!(page1.len(), 5);

        // Page 2: 5 messages before the oldest of page 1 — use the compound cursor
        let cursor_ts = page1[0].created_at;
        let cursor_id = page1[0].id.as_str();
        let page2 = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            Some(cursor_ts),
            Some(cursor_id),
            Some(5),
        )
        .await
        .unwrap();
        assert_eq!(page2.len(), 5);

        // No overlap between pages
        let page1_ids: std::collections::HashSet<_> = page1.iter().map(|m| &m.id).collect();
        let page2_ids: std::collections::HashSet<_> = page2.iter().map(|m| &m.id).collect();
        assert!(page1_ids.is_disjoint(&page2_ids), "pages must not overlap");

        // Together they cover all 10 messages
        let mut all_ids: Vec<_> = page1_ids.into_iter().chain(page2_ids).collect();
        all_ids.sort();
        assert_eq!(all_ids.len(), 10, "pages together must cover all messages");

        // Page 3: cursor past the oldest of page 2 — should be empty
        let cursor_ts2 = page2[0].created_at;
        let cursor_id2 = page2[0].id.as_str();
        let page3 = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            Some(cursor_ts2),
            Some(cursor_id2),
            Some(5),
        )
        .await
        .unwrap();
        assert!(page3.is_empty(), "page beyond history should be empty");
    }

    /// Regression test: multiple messages sharing the same `created_at` second span a page
    /// boundary. Without compound cursor tie-breaking, rows at the boundary second are
    /// non-deterministically included or excluded, leading to skipped or duplicated messages
    /// across pages. This test verifies that every message is returned exactly once.
    #[tokio::test]
    async fn test_paginated_tied_timestamps_no_skip_or_duplicate_across_pages() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[206; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();

        // 6 messages at the SAME second (the tie), plus one earlier and one later
        // to ensure the cursor logic handles boundary conditions on both sides.
        let tie_ts: u64 = 1_700_500_000;

        // Seeds 1–6: all at `tie_ts` (identical created_at seconds)
        // Seeds 7–8: flanking timestamps to confirm ordering is correct
        insert_message_at(7, author, tie_ts - 1, &group_id, &whitenoise.database).await; // earlier
        for i in 1u8..=6 {
            insert_message_at(i, author, tie_ts, &group_id, &whitenoise.database).await;
        }
        insert_message_at(8, author, tie_ts + 1, &group_id, &whitenoise.database).await; // later

        // Total 8 messages. Page size 3 — three pages needed.
        // Page 1: newest 3
        let page1 = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            None,
            None,
            Some(3),
        )
        .await
        .unwrap();
        assert_eq!(page1.len(), 3, "page 1 should have 3 messages");

        // Page 2: compound cursor from oldest of page 1
        let c1_ts = page1[0].created_at;
        let c1_id = page1[0].id.clone();
        let page2 = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            Some(c1_ts),
            Some(c1_id.as_str()),
            Some(3),
        )
        .await
        .unwrap();
        assert_eq!(page2.len(), 3, "page 2 should have 3 messages");

        // Page 3: compound cursor from oldest of page 2
        let c2_ts = page2[0].created_at;
        let c2_id = page2[0].id.clone();
        let page3 = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            Some(c2_ts),
            Some(c2_id.as_str()),
            Some(3),
        )
        .await
        .unwrap();
        assert_eq!(
            page3.len(),
            2,
            "page 3 should have the remaining 2 messages"
        );

        // Page 4: should be empty — history exhausted
        let c3_ts = page3[0].created_at;
        let c3_id = page3[0].id.clone();
        let page4 = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            Some(c3_ts),
            Some(c3_id.as_str()),
            Some(3),
        )
        .await
        .unwrap();
        assert!(page4.is_empty(), "page 4 should be empty");

        // Collect all returned IDs across all pages
        let all_returned: std::collections::HashSet<String> = page1
            .iter()
            .chain(page2.iter())
            .chain(page3.iter())
            .map(|m| m.id.clone())
            .collect();

        // Verify: exactly 8 unique messages, no skips, no duplicates
        assert_eq!(
            all_returned.len(),
            8,
            "all 8 messages must be returned exactly once across pages"
        );

        // Verify oldest-first ordering within each page
        for (page_num, page) in [&page1, &page2, &page3].iter().enumerate() {
            for w in page.windows(2) {
                assert!(
                    w[0].created_at < w[1].created_at
                        || (w[0].created_at == w[1].created_at && w[0].id <= w[1].id),
                    "page {} must be ordered by (created_at, id) ascending",
                    page_num + 1
                );
            }
        }
    }

    #[tokio::test]
    async fn test_paginated_half_specified_cursor_is_rejected() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[207; 32]);
        let ts = Timestamp::from(1_700_000_000u64);

        // before=Some, before_message_id=None → InvalidCursor
        let err = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            Some(ts),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, DatabaseError::InvalidCursor { .. }),
            "expected InvalidCursor, got: {err}"
        );

        // before=None, before_message_id=Some → InvalidCursor
        let err = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            None,
            Some("0000000000000000000000000000000000000000000000000000000000000001"),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, DatabaseError::InvalidCursor { .. }),
            "expected InvalidCursor, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_paginated_invalid_before_message_id_is_rejected() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[208; 32]);
        let ts = Timestamp::from(1_700_000_000u64);

        // Non-hex string → InvalidCursor
        let err = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            Some(ts),
            Some("not-valid-hex-at-all"),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, DatabaseError::InvalidCursor { .. }),
            "expected InvalidCursor for non-hex id, got: {err}"
        );

        // Valid hex but wrong length (32 chars instead of 64) → InvalidCursor
        let err = AggregatedMessage::find_messages_by_group_paginated(
            &group_id,
            &whitenoise.database,
            Some(ts),
            Some("00000000000000000000000000000001"),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, DatabaseError::InvalidCursor { .. }),
            "expected InvalidCursor for short hex id, got: {err}"
        );
    }

    fn create_test_chat_message_with_content(
        seed: u8,
        author: PublicKey,
        content: &str,
    ) -> ChatMessage {
        let id = format!("{:0>64}", format!("{:x}", seed));
        ChatMessage {
            id,
            author,
            content: content.to_string(),
            created_at: Timestamp::from(1_700_000_000u64 + seed as u64),
            tags: Tags::new(),
            is_reply: false,
            reply_to_id: None,
            is_deleted: false,
            content_tokens: vec![],
            reactions: ReactionSummary::default(),
            kind: 9,
            media_attachments: vec![],
            delivery_status: None,
        }
    }

    #[tokio::test]
    async fn test_search_messages_in_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[42; 32]);
        setup_group(&group_id, &whitenoise.database).await;

        let author = Keys::generate().public_key();

        let messages = vec![
            create_test_chat_message_with_content(1, author, "hello world"),
            create_test_chat_message_with_content(2, author, "marmot protocol is the future"),
            create_test_chat_message_with_content(
                3,
                author,
                "our big plans are bigger than you imagine",
            ),
            create_test_chat_message_with_content(4, author, "日本語のメッセージ"),
            create_test_chat_message_with_content(5, author, "привет мир from the colony"),
            create_test_chat_message_with_content(6, author, "नमस्ते दुनिया"),
        ];

        for msg in &messages {
            AggregatedMessage::insert_message(msg, &group_id, &whitenoise.database)
                .await
                .unwrap();
        }

        // Basic single-word search
        let results = AggregatedMessage::search_messages_in_group(
            &group_id,
            "hello",
            50,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message.content, "hello world");

        // Forward-order multi-word search
        let results = AggregatedMessage::search_messages_in_group(
            &group_id,
            "big plans",
            50,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].message.content,
            "our big plans are bigger than you imagine"
        );

        // Substring matching ("big" matches "bigger")
        let results =
            AggregatedMessage::search_messages_in_group(&group_id, "big", 50, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(results.len(), 1);

        // Case insensitive
        let results = AggregatedMessage::search_messages_in_group(
            &group_id,
            "MARMOT",
            50,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message.content, "marmot protocol is the future");

        // CJK search
        let results = AggregatedMessage::search_messages_in_group(
            &group_id,
            "日本語",
            50,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message.content, "日本語のメッセージ");

        // Cyrillic search
        let results = AggregatedMessage::search_messages_in_group(
            &group_id,
            "привет",
            50,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 1);

        // Devanagari search (with combining marks)
        let results = AggregatedMessage::search_messages_in_group(
            &group_id,
            "नमस्ते",
            50,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message.content, "नमस्ते दुनिया");

        // No match
        let results = AggregatedMessage::search_messages_in_group(
            &group_id,
            "nonexistent",
            50,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(results.is_empty());

        // Empty query matches all
        let results =
            AggregatedMessage::search_messages_in_group(&group_id, "", 50, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(results.len(), 6);

        // Limit is respected
        let results =
            AggregatedMessage::search_messages_in_group(&group_id, "", 2, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(results.len(), 2);
    }
}

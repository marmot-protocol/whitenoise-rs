use super::{Database, DatabaseError};
use crate::marmot::{GroupId, Message};
use crate::perf_instrument;

type Result<T> = std::result::Result<T, DatabaseError>;

pub(crate) struct MarmotMessageProjection;

impl MarmotMessageProjection {
    #[perf_instrument("db::marmot_messages")]
    pub(crate) async fn upsert(message: &Message, database: &Database) -> Result<()> {
        sqlx::query(
            "INSERT INTO marmot_message_projections
             (message_id, mls_group_id, created_at, processed_at, message_json)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(message_id, mls_group_id) DO UPDATE SET
                created_at = excluded.created_at,
                processed_at = excluded.processed_at,
                message_json = excluded.message_json",
        )
        .bind(message.id.to_hex())
        .bind(message.mls_group_id.as_slice())
        .bind(timestamp_ms(message.created_at.as_secs()))
        .bind(timestamp_ms(message.processed_at.as_secs()))
        .bind(serde_json::to_string(message)?)
        .execute(&database.pool)
        .await?;

        Ok(())
    }

    #[perf_instrument("db::marmot_messages")]
    pub(crate) async fn list_by_group(
        group_id: &GroupId,
        database: &Database,
    ) -> Result<Vec<Message>> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT message_json
             FROM marmot_message_projections
             WHERE mls_group_id = ?
             ORDER BY created_at DESC, processed_at DESC, message_id DESC",
        )
        .bind(group_id.as_slice())
        .fetch_all(&database.pool)
        .await?;

        rows.into_iter()
            .map(|row| serde_json::from_str(&row).map_err(DatabaseError::from))
            .collect()
    }

    #[perf_instrument("db::marmot_messages")]
    pub(crate) async fn delete_by_group(group_id: &GroupId, database: &Database) -> Result<()> {
        sqlx::query("DELETE FROM marmot_message_projections WHERE mls_group_id = ?")
            .bind(group_id.as_slice())
            .execute(&database.pool)
            .await?;

        Ok(())
    }
}

fn timestamp_ms(seconds: u64) -> i64 {
    seconds.saturating_mul(1000).min(i64::MAX as u64) as i64
}

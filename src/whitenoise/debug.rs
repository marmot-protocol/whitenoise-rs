use mdk_core::prelude::RatchetTreeInfo;
use mdk_storage_traits::GroupId;
use sqlx::{Column, Row, TypeInfo, ValueRef};

use super::Whitenoise;
use super::accounts::Account;
use super::error::{Result, WhitenoiseError};

impl Whitenoise {
    /// Executes an arbitrary SQL query and returns the raw results as a JSON string.
    ///
    /// This is a **debug-only** method intended for development and troubleshooting.
    /// It runs the given SQL against the application database and serializes all
    /// returned rows into a JSON array of objects, where each object maps column
    /// names to their values.
    ///
    /// # Column type mapping
    ///
    /// SQLite values are mapped to JSON as follows:
    /// - `INTEGER` → JSON number
    /// - `REAL` → JSON number
    /// - `TEXT` → JSON string
    /// - `BLOB` → JSON string (hex-encoded, prefixed with `"0x"`)
    /// - `NULL` → JSON null
    ///
    /// # Arguments
    ///
    /// * `sql` - Any valid SQL statement. Both read and write queries are accepted.
    ///
    /// # Returns
    ///
    /// A JSON string representing an array of row objects, e.g.:
    ///
    /// ```json
    /// [
    ///   {"id": 1, "name": "alice", "created_at": 1700000000},
    ///   {"id": 2, "name": "bob",   "created_at": 1700000001}
    /// ]
    /// ```
    ///
    /// For write statements (`INSERT`, `UPDATE`, `DELETE`) that return no rows,
    /// the result is an empty array `"[]"`.
    /// Returns public information about the ratchet tree of an MLS group.
    ///
    /// Exposes the MLS ratchet tree structure for a given group. The returned
    /// data contains only public information (encryption keys, signature keys,
    /// tree structure) — no secrets.
    ///
    /// # Arguments
    ///
    /// * `account` - The account that is a member of the group
    /// * `group_id` - The MLS group ID to inspect
    ///
    /// # Returns
    ///
    /// A [`RatchetTreeInfo`] containing the tree hash, serialized tree, and leaf nodes.
    pub fn ratchet_tree_info(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<RatchetTreeInfo> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        mdk.get_ratchet_tree_info(group_id)
            .map_err(WhitenoiseError::from)
    }

    pub async fn debug_query(&self, sql: &str) -> Result<String> {
        tracing::warn!(
            target: "whitenoise::debug",
            "Executing debug query: {}",
            sql
        );

        let rows = sqlx::query(sql).fetch_all(&self.database.pool).await?;

        let mut result: Vec<serde_json::Value> = Vec::with_capacity(rows.len());

        for row in &rows {
            let columns = row.columns();
            let mut map = serde_json::Map::with_capacity(columns.len());

            for column in columns {
                let name = column.name().to_string();
                let value = sqlite_value_to_json(row, column.ordinal());
                map.insert(name, value);
            }

            result.push(serde_json::Value::Object(map));
        }

        let json = serde_json::to_string(&result)?;
        Ok(json)
    }
}

/// Converts a single SQLite column value to a [`serde_json::Value`].
///
/// Inspects the raw SQLite type name reported by the value reference to decide
/// the correct conversion. This avoids false positives from `try_get` coercing
/// `NULL` into a default like `0`.
fn sqlite_value_to_json(row: &sqlx::sqlite::SqliteRow, ordinal: usize) -> serde_json::Value {
    let raw = row.try_get_raw(ordinal);
    let Ok(value_ref) = raw else {
        return serde_json::Value::Null;
    };

    if value_ref.is_null() {
        return serde_json::Value::Null;
    }

    let type_info = value_ref.type_info();
    let type_name = type_info.name();

    match type_name {
        "INTEGER" | "INT" | "BIGINT" | "SMALLINT" | "TINYINT" | "BOOLEAN" => {
            if let Ok(v) = row.try_get::<i64, _>(ordinal) {
                return serde_json::Value::Number(v.into());
            }
        }
        "REAL" | "FLOAT" | "DOUBLE" => {
            if let Ok(v) = row.try_get::<f64, _>(ordinal)
                && let Some(n) = serde_json::Number::from_f64(v)
            {
                return serde_json::Value::Number(n);
            }
        }
        "TEXT" | "VARCHAR" | "CLOB" => {
            if let Ok(v) = row.try_get::<String, _>(ordinal) {
                return serde_json::Value::String(v);
            }
        }
        "BLOB" => {
            if let Ok(v) = row.try_get::<Vec<u8>, _>(ordinal) {
                let hex = v.iter().fold(String::from("0x"), |mut acc, byte| {
                    use std::fmt::Write;
                    let _ = write!(acc, "{byte:02x}");
                    acc
                });
                return serde_json::Value::String(hex);
            }
        }
        "NULL" => return serde_json::Value::Null,
        // Unknown type: try common conversions in order
        _ => {
            if let Ok(v) = row.try_get::<i64, _>(ordinal) {
                return serde_json::Value::Number(v.into());
            }
            if let Ok(v) = row.try_get::<f64, _>(ordinal)
                && let Some(n) = serde_json::Number::from_f64(v)
            {
                return serde_json::Value::Number(n);
            }
            if let Ok(v) = row.try_get::<String, _>(ordinal) {
                return serde_json::Value::String(v);
            }
            if let Ok(v) = row.try_get::<Vec<u8>, _>(ordinal) {
                let hex = v.iter().fold(String::from("0x"), |mut acc, byte| {
                    use std::fmt::Write;
                    let _ = write!(acc, "{byte:02x}");
                    acc
                });
                return serde_json::Value::String(hex);
            }
        }
    }

    serde_json::Value::Null
}

#[cfg(test)]
mod tests {
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    #[tokio::test]
    async fn debug_query_returns_json_array() {
        let (wn, _data_dir, _logs_dir) = create_mock_whitenoise().await;
        let json = wn.debug_query("SELECT 1 AS value").await.unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["value"], 1);
    }

    #[tokio::test]
    async fn debug_query_empty_result() {
        let (wn, _data_dir, _logs_dir) = create_mock_whitenoise().await;
        let json = wn
            .debug_query("SELECT * FROM accounts WHERE 1 = 0")
            .await
            .unwrap();
        assert_eq!(json, "[]");
    }

    #[tokio::test]
    async fn debug_query_handles_null() {
        let (wn, _data_dir, _logs_dir) = create_mock_whitenoise().await;
        let json = wn.debug_query("SELECT NULL AS empty").await.unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert!(parsed[0]["empty"].is_null());
    }

    #[tokio::test]
    async fn debug_query_returns_error_for_invalid_sql() {
        let (wn, _data_dir, _logs_dir) = create_mock_whitenoise().await;
        let result = wn.debug_query("THIS IS NOT SQL").await;
        assert!(result.is_err());
    }
}

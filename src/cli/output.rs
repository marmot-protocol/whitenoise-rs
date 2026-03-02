use std::borrow::Cow;
use std::io::{self, Write};

use nostr_sdk::{PublicKey, ToBech32};

use super::protocol::Response;

/// Print a response and return a non-zero exit code on error.
pub fn print_and_exit(response: &Response, json: bool) -> anyhow::Result<()> {
    print_response(response, json);
    if response.error.is_some() {
        std::process::exit(1);
    }
    Ok(())
}

/// Print a response to stdout, either as JSON or human-readable.
pub fn print_response(response: &Response, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(response).unwrap_or_default()
        );
        return;
    }

    if let Some(error) = &response.error {
        eprintln!("Error: {}", error.message);
        return;
    }

    if let Some(value) = &response.result {
        let mut out = io::stdout().lock();
        format_value(&mut out, value, 0).ok();
    }
}

/// Field names whose values should be displayed as hex-encoded bytes.
const HEX_FIELDS: &[&str] = &["mls_group_id", "nostr_group_id"];

/// Field names whose hex values should NOT be converted to npub (they are event IDs, not pubkeys).
const NO_NPUB_FIELDS: &[&str] = &["id", "reply_to", "message_id"];

/// Fields hidden entirely in human-readable output (still present in --json).
const HIDDEN_FIELDS: &[&str] = &["author", "created_at", "user_reactions", "reaction_id"];

fn format_value(w: &mut impl Write, value: &serde_json::Value, indent: usize) -> io::Result<()> {
    let prefix = "  ".repeat(indent);
    match value {
        serde_json::Value::Null => Ok(()),
        serde_json::Value::String(s) => writeln!(w, "{prefix}{s}"),
        serde_json::Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                if i > 0 {
                    writeln!(w)?;
                }
                format_value(w, item, indent)?;
            }
            Ok(())
        }
        serde_json::Value::Object(map) => format_object(w, map, indent),
        other => writeln!(w, "{prefix}{other}"),
    }
}

fn format_object(
    w: &mut impl Write,
    map: &serde_json::Map<String, serde_json::Value>,
    indent: usize,
) -> io::Result<()> {
    let prefix = "  ".repeat(indent);
    for (key, val) in map {
        // Skip fields hidden in human-readable output
        if HIDDEN_FIELDS.contains(&key.as_str()) {
            continue;
        }

        // Skip false booleans and default values
        if val == &serde_json::Value::Bool(false) {
            continue;
        }
        if key == "kind" && val.as_u64() == Some(9) {
            continue;
        }

        // Check hex fields first
        if HEX_FIELDS.contains(&key.as_str())
            && let Some(hex) = try_as_hex(val)
        {
            writeln!(w, "{prefix}{key}: {hex}")?;
            continue;
        }

        match val {
            serde_json::Value::Null => {}
            serde_json::Value::String(s) if s.is_empty() => {}
            serde_json::Value::String(s) => {
                let display = if NO_NPUB_FIELDS.contains(&key.as_str()) {
                    Cow::Borrowed(s.as_str())
                } else {
                    maybe_npub(s)
                };
                writeln!(w, "{prefix}{key}: {display}")?;
            }
            serde_json::Value::Object(nested) if nested.is_empty() => {}
            serde_json::Value::Object(nested) => {
                writeln!(w, "{prefix}{key}:")?;
                format_object(w, nested, indent + 1)?;
            }
            serde_json::Value::Array(arr) if arr.is_empty() => {}
            serde_json::Value::Array(arr) if arr.iter().all(|v| v.is_string()) => {
                let no_npub = NO_NPUB_FIELDS.contains(&key.as_str());
                let formatted: Vec<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| {
                        if no_npub {
                            s.to_string()
                        } else {
                            maybe_npub(s).into_owned()
                        }
                    })
                    .collect();
                writeln!(w, "{prefix}{key}: {}", formatted.join(", "))?;
            }
            serde_json::Value::Array(arr) if arr.iter().all(|v| v.is_object()) => {
                writeln!(w, "{prefix}{key}:")?;
                for (i, item) in arr.iter().enumerate() {
                    if i > 0 {
                        writeln!(w)?;
                    }
                    format_value(w, item, indent + 1)?;
                }
            }
            other => writeln!(w, "{prefix}{key}: {other}")?,
        }
    }
    Ok(())
}

/// If the string is a valid hex pubkey, return its npub encoding. Otherwise return as-is.
fn maybe_npub(s: &str) -> Cow<'_, str> {
    match PublicKey::from_hex(s) {
        Ok(pk) => Cow::Owned(pk.to_bech32().unwrap()),
        Err(_) => Cow::Borrowed(s),
    }
}

/// Try to extract a byte array from a JSON value and return it as a hex string.
///
/// Handles two patterns:
/// - Plain array: `[100, 88, ...]`
/// - MLS wrapper: `{"value": {"vec": [92, 112, ...]}}`
fn try_as_hex(value: &serde_json::Value) -> Option<String> {
    let arr = if let Some(arr) = value.as_array() {
        arr
    } else {
        // MLS GroupId wrapper: {"value": {"vec": [...]}}
        value.get("value")?.get("vec")?.as_array()?
    };

    let bytes: Option<Vec<u8>> = arr
        .iter()
        .map(|v| v.as_u64().and_then(|n| u8::try_from(n).ok()))
        .collect();
    Some(hex::encode(bytes?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(value: serde_json::Value) -> String {
        let mut buf = Vec::new();
        format_value(&mut buf, &value, 0).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn string_value() {
        assert_eq!(render(json!("hello")), "hello\n");
    }

    #[test]
    fn null_produces_no_output() {
        assert_eq!(render(json!(null)), "");
    }

    #[test]
    fn object_key_value_pairs() {
        let out = render(json!({"name": "alice", "role": "admin"}));
        assert!(out.contains("name: alice\n"));
        assert!(out.contains("role: admin\n"));
    }

    #[test]
    fn object_skips_null_fields() {
        let out = render(json!({"name": "alice", "email": null}));
        assert!(out.contains("name: alice"));
        assert!(!out.contains("email"));
    }

    #[test]
    fn array_items_separated_by_blank_line() {
        let out = render(json!([{"a": "1"}, {"b": "2"}]));
        assert!(out.contains("a: 1\n\nb: 2\n"));
    }

    #[test]
    fn single_item_array_no_blank_line() {
        let out = render(json!([{"a": "1"}]));
        assert_eq!(out, "a: 1\n");
    }

    #[test]
    fn numeric_value_in_object() {
        let out = render(json!({"count": 42}));
        assert_eq!(out, "count: 42\n");
    }

    #[test]
    fn nostr_group_id_displays_as_hex() {
        let out = render(json!({"nostr_group_id": [0x64, 0x58, 0x3a, 0x39]}));
        assert_eq!(out, "nostr_group_id: 64583a39\n");
    }

    #[test]
    fn mls_group_id_displays_as_hex() {
        let out = render(json!({"mls_group_id": {"value": {"vec": [0x5c, 0x70, 0x37, 0x1e]}}}));
        assert_eq!(out, "mls_group_id: 5c70371e\n");
    }

    #[test]
    fn non_byte_array_unchanged() {
        let out = render(json!({"other_field": [1, 2, 3]}));
        assert_eq!(out, "other_field: [1,2,3]\n");
    }

    #[test]
    fn nested_object_recurses() {
        let out = render(json!({"group": {"name": "Chat", "epoch": 0}}));
        assert!(out.contains("group:\n"));
        assert!(out.contains("  name: Chat\n"));
        assert!(out.contains("  epoch: 0\n"));
    }

    #[test]
    fn hex_fields_inside_nested_object() {
        let out = render(json!({
            "group": {
                "name": "Chat",
                "mls_group_id": {"value": {"vec": [0xab, 0xcd]}},
                "nostr_group_id": [0xef, 0x01]
            }
        }));
        assert!(out.contains("  mls_group_id: abcd\n"));
        assert!(out.contains("  nostr_group_id: ef01\n"));
    }

    #[test]
    fn pubkey_string_displays_as_npub() {
        let hex = "4dd7a05f5f668589d5d3025a30e3a2603f2d4fe7fb9d0e2b33914b765e9d6f69";
        let out = render(json!({"account_pubkey": hex}));
        assert!(out.contains("account_pubkey: npub1"));
        assert!(!out.contains(hex));
    }

    #[test]
    fn pubkey_array_displays_as_npubs() {
        let hex1 = "4dd7a05f5f668589d5d3025a30e3a2603f2d4fe7fb9d0e2b33914b765e9d6f69";
        let hex2 = "0000000000000000000000000000000000000000000000000000000000000001";
        let out = render(json!({"admin_pubkeys": [hex1, hex2]}));
        assert!(out.contains("npub1"));
        assert!(!out.contains(hex1));
        assert!(!out.contains(hex2));
    }

    #[test]
    fn non_pubkey_string_unchanged() {
        let out = render(json!({"name": "alice"}));
        assert_eq!(out, "name: alice\n");
    }

    #[test]
    fn object_skips_empty_string_fields() {
        let out = render(json!({"name": "alice", "bio": ""}));
        assert!(out.contains("name: alice"));
        assert!(!out.contains("bio"));
    }

    #[test]
    fn object_skips_empty_array_fields() {
        let out = render(json!({"name": "alice", "tags": []}));
        assert!(out.contains("name: alice"));
        assert!(!out.contains("tags"));
    }

    #[test]
    fn object_skips_empty_nested_object_fields() {
        let out = render(json!({"name": "alice", "meta": {}}));
        assert!(out.contains("name: alice"));
        assert!(!out.contains("meta"));
    }

    #[test]
    fn hidden_fields_not_shown() {
        let out = render(json!({"display_name": "alice", "author": "deadbeef"}));
        assert!(out.contains("display_name: alice"));
        assert!(!out.contains("author"));
    }

    #[test]
    fn false_booleans_hidden() {
        let out = render(json!({"name": "alice", "is_reply": false}));
        assert!(out.contains("name: alice"));
        assert!(!out.contains("is_reply"));
    }

    #[test]
    fn true_booleans_shown() {
        let out = render(json!({"is_reply": true}));
        assert!(out.contains("is_reply: true"));
    }

    #[test]
    fn default_kind_9_hidden() {
        let out = render(json!({"content": "hi", "kind": 9}));
        assert!(out.contains("content: hi"));
        assert!(!out.contains("kind"));
    }

    #[test]
    fn non_default_kind_shown() {
        let out = render(json!({"content": "hi", "kind": 7}));
        assert!(out.contains("kind: 7"));
    }

    #[test]
    fn id_field_stays_hex_not_npub() {
        let hex = "4dd7a05f5f668589d5d3025a30e3a2603f2d4fe7fb9d0e2b33914b765e9d6f69";
        let out = render(json!({"id": hex}));
        assert!(out.contains(hex), "id field should stay as hex");
        assert!(!out.contains("npub1"), "id field should not become npub");
    }

    #[test]
    fn reply_to_field_stays_hex_not_npub() {
        let hex = "4dd7a05f5f668589d5d3025a30e3a2603f2d4fe7fb9d0e2b33914b765e9d6f69";
        let out = render(json!({"reply_to": hex}));
        assert!(out.contains(hex));
        assert!(!out.contains("npub1"));
    }

    #[test]
    fn message_id_field_stays_hex_not_npub() {
        let hex = "4dd7a05f5f668589d5d3025a30e3a2603f2d4fe7fb9d0e2b33914b765e9d6f69";
        let out = render(json!({"message_id": hex}));
        assert!(out.contains(hex), "message_id should stay as hex");
        assert!(!out.contains("npub1"), "message_id should not become npub");
    }

    #[test]
    fn array_of_nested_objects() {
        let out = render(json!([
            {"group": {"name": "A"}, "id": 1},
            {"group": {"name": "B"}, "id": 2}
        ]));
        assert!(out.contains("group:\n"));
        assert!(out.contains("  name: A\n"));
        assert!(out.contains("  name: B\n"));
        // Items separated by blank line
        assert!(out.contains("\n\n"));
    }
}

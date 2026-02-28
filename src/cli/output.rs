use std::io::{self, Write};

use super::protocol::Response;

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
        format_value(&mut out, value).ok();
    }
}

fn format_value(w: &mut impl Write, value: &serde_json::Value) -> io::Result<()> {
    match value {
        serde_json::Value::Null => Ok(()),
        serde_json::Value::String(s) => writeln!(w, "{s}"),
        serde_json::Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                if i > 0 {
                    writeln!(w)?;
                }
                format_value(w, item)?;
            }
            Ok(())
        }
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                match val {
                    serde_json::Value::Null => {}
                    serde_json::Value::String(s) => writeln!(w, "{key}: {s}")?,
                    other => writeln!(w, "{key}: {other}")?,
                }
            }
            Ok(())
        }
        other => writeln!(w, "{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(value: serde_json::Value) -> String {
        let mut buf = Vec::new();
        format_value(&mut buf, &value).unwrap();
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
}

use serde_json::Value;
use std::path::Path;

/// Extract a `f64` field from a JSON object.
pub fn get_f64(v: &Value, key: &str) -> Option<f64> {
    v.get(key)?.as_f64()
}

/// Extract a `String` field from a JSON object, defaulting to `"unknown"`.
pub fn get_str(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Extract a `u64` field from a JSON object, defaulting to `0`.
pub fn get_u64(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Read and parse a JSON file, returning a human-readable error string on failure.
pub fn load_json(path: &Path) -> Result<Value, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("Failed to parse {}: {e}", path.display()))
}

/// Format a nanosecond value as a human-readable duration string.
pub fn format_ns(ns: f64) -> String {
    if ns >= 1_000_000_000.0 {
        format!("{:.2}s", ns / 1_000_000_000.0)
    } else if ns >= 1_000_000.0 {
        format!("{:.1}ms", ns / 1_000_000.0)
    } else if ns >= 1_000.0 {
        format!("{:.1}µs", ns / 1_000.0)
    } else {
        format!("{:.0}ns", ns)
    }
}

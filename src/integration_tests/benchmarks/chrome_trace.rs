//! Chrome Trace Event / Perfetto JSON export.
//!
//! Converts `--detailed` benchmark results into a JSON trace file that can be
//! opened in `chrome://tracing` or [ui.perfetto.dev](https://ui.perfetto.dev).
//!
//! Format spec: <https://docs.google.com/document/d/1CvAClvFfyA5R-PhYUmn5OOQtYMH4h6I0nSsKchNAySU>
//!
//! Mapping:
//! - `pid` = scenario index — groups scenarios as separate "processes" in the UI.
//! - `tid` = `trace_id` — groups spans from the same logical operation as threads.
//! - `ts` / `dur` — microseconds (Perfetto standard).
//! - `args.iteration` — allows filtering by benchmark iteration in the UI.

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::integration_tests::benchmarks::BenchmarkResult;

/// A single Perfetto "X" (complete) event.
#[derive(Serialize)]
struct TraceEvent {
    name: String,
    ph: &'static str,
    ts: f64,
    dur: f64,
    pid: usize,
    tid: u64,
    args: TraceArgs,
}

#[derive(Serialize)]
struct TraceArgs {
    trace_id: u64,
    iteration: usize,
}

/// Top-level Perfetto JSON envelope.
#[derive(Serialize)]
struct PerfettoTrace {
    #[serde(rename = "traceEvents")]
    trace_events: Vec<Value>,
    /// Descriptive metadata shown in the Perfetto UI.
    #[serde(rename = "displayTimeUnit")]
    display_time_unit: &'static str,
}

/// Build and write a Perfetto trace file from `--detailed` benchmark results.
///
/// Each scenario becomes a separate PID in the trace, and each unique `trace_id`
/// within that scenario becomes a separate TID (thread lane). Spans with
/// `ts_begin_us == 0` (e.g. sqlx events that don't emit a timestamp) are skipped
/// — they cannot be anchored on the real-time axis.
///
/// Returns `Ok(())` on success. Returns an error if no `per_iteration` data is
/// present (i.e. the binary was not run with `--detailed`).
pub fn write_perfetto_trace(
    path: &Path,
    results: &[BenchmarkResult],
) -> Result<(), crate::WhitenoiseError> {
    let has_detail = results.iter().any(|r| r.per_iteration.is_some());
    if !has_detail {
        return Err(crate::WhitenoiseError::InvalidInput(
            "--chrome-trace requires --detailed; no per-iteration data found".to_string(),
        ));
    }

    let mut events: Vec<Value> = Vec::new();

    for (scenario_idx, result) in results.iter().enumerate() {
        let pid = scenario_idx + 1; // 1-based so pid=0 is never used

        // Emit process name metadata event so Perfetto labels the PID correctly.
        events.push(serde_json::json!({
            "name": "process_name",
            "ph": "M",
            "pid": pid,
            "tid": 0,
            "args": { "name": &result.name },
        }));

        let Some(ref iterations) = result.per_iteration else {
            continue;
        };

        for detail in iterations {
            for span in &detail.spans {
                // Skip events that have no real timestamp (sqlx, etc.)
                if span.ts_begin_us == 0 {
                    continue;
                }
                let ts_us = span.ts_begin_us as f64;
                let dur_us = span.duration.as_nanos() as f64 / 1_000.0;

                let event = TraceEvent {
                    name: span.name.clone(),
                    ph: "X",
                    ts: ts_us,
                    dur: dur_us,
                    pid,
                    tid: span.trace_id,
                    args: TraceArgs {
                        trace_id: span.trace_id,
                        iteration: detail.iteration,
                    },
                };
                events.push(serde_json::to_value(event)?);
            }
        }
    }

    let trace = PerfettoTrace {
        trace_events: events,
        display_time_unit: "ms",
    };

    let json = serde_json::to_string_pretty(&trace)?;
    std::fs::write(path, json)?;

    tracing::info!("Perfetto trace written to {}", path.display());
    tracing::info!("Open at: https://ui.perfetto.dev");
    Ok(())
}

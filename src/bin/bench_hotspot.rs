//! bench_hotspot — rank performance hotspots from benchmark JSON output.
//!
//! Usage:
//! ```text
//! bench_hotspot <results.json>
//!     --history-dir PATH      # path to perf-hotspot/hotspots/ branch checkout
//!     --output-json PATH      # write today's hotspot JSON here
//!     [--top-n 10]            # number of hotspots to surface (default 10)
//!     [--run-id ID]           # timestamp string used in output filenames
//! ```
//!
//! Reads the full benchmark JSON (WU1 format), applies filters, scores spans,
//! computes week-over-week trends from history, writes a hotspot JSON and
//! prints a human-readable ranked table.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{NaiveDateTime, Utc};

use clap::Parser;
use serde::{Deserialize, Serialize};

use ::whitenoise::integration_tests::benchmarks::bench_utils::{
    format_ns, get_f64, get_str, get_u64, load_json,
};

#[derive(Parser, Debug)]
#[clap(
    author,
    version,
    about = "Rank performance hotspots from benchmark results"
)]
struct Args {
    /// Path to the benchmark JSON results file.
    results: PathBuf,

    /// Path to the hotspot history directory (perf-hotspot/hotspots/).
    #[clap(long, value_name = "PATH")]
    history_dir: PathBuf,

    /// Write hotspot JSON output to this path.
    #[clap(long, value_name = "PATH")]
    output_json: PathBuf,

    /// Number of hotspot spans to surface. Default: 10.
    #[clap(long, default_value = "10")]
    top_n: usize,

    /// Run ID (timestamp string) embedded in the output for traceability.
    #[clap(long, value_name = "ID")]
    run_id: Option<String>,
}

// ─── Filtering constants ──────────────────────────────────────────────────────

/// Minimum mean duration for a span to be considered a hotspot.
const MIN_MEAN_NS: f64 = 10_000_000.0; // 10ms

/// Minimum contribution percentage (span.total_ns / scenario.total_ns * 100).
const MIN_CONTRIBUTION_PCT: f64 = 1.0;

/// Maximum contribution percentage — drops outermost container spans.
const MAX_CONTRIBUTION_PCT: f64 = 95.0;

// ─── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HotspotEntry {
    rank: usize,
    span_name: String,
    scenario_name: String,
    mean_ns: f64,
    p95_ns: f64,
    total_ns: f64,
    call_count: u64,
    contribution_pct: f64,
    hotspot_score: f64,
    /// Week-over-week trend in percent. `None` during the first 7 days.
    trend_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HotspotOutput {
    run_id: String,
    top_n: usize,
    hotspots: Vec<HotspotEntry>,
}

// ─── Scoring ─────────────────────────────────────────────────────────────────

/// Hotspot score: relative duration × log-scaled call frequency.
///
/// ```text
/// hotspot_score = (mean_ns / scenario_mean_ns) * ln_1p(call_count)
/// ```
fn hotspot_score(mean_ns: f64, scenario_mean_ns: f64, call_count: u64) -> f64 {
    if scenario_mean_ns == 0.0 {
        return 0.0;
    }
    (mean_ns / scenario_mean_ns) * (call_count as f64).ln_1p()
}

// ─── Trend lookup ─────────────────────────────────────────────────────────────

/// Parse a `run_id` string of the form `YYYY-MM-DDTHHMMSSZ` (produced by
/// `date -u +%Y-%m-%dT%H%M%SZ`) into seconds since the Unix epoch.
/// Returns `None` if the string doesn't match the expected format.
fn run_id_to_secs(s: &str) -> Option<u64> {
    let dt = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H%M%SZ").ok()?;
    let ts = dt.and_utc().timestamp();
    if ts < 0 { None } else { Some(ts as u64) }
}

/// Load the hotspot JSON file from exactly 7 days ago (approximate — finds the
/// newest file whose logical run timestamp is older than 7 × 24 hours) and
/// build a lookup map: `scenario_name → span_name → mean_ns`.
///
/// The logical timestamp is read from the `run_id` field inside each JSON
/// file. If that field is absent or unparseable the filename stem is tried
/// as a fallback (both use the same `YYYY-MM-DDTHHMMSSZ` format). File
/// modification time is intentionally not used: `git checkout` in CI resets
/// mtimes to the checkout instant, making them useless for ordering.
fn load_week_ago_means(history_dir: &Path) -> HashMap<String, HashMap<String, f64>> {
    let threshold = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(7 * 24 * 3600);

    let mut best: Option<(u64, PathBuf)> = None;

    if let Ok(entries) = std::fs::read_dir(history_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            // Derive logical timestamp: try run_id field in the report first,
            // then fall back to parsing the filename stem.
            let ts = load_json(&path)
                .ok()
                .and_then(|j| run_id_to_secs(&get_str(&j, "run_id")))
                .or_else(|| {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .and_then(run_id_to_secs)
                });

            if let Some(ts) = ts
                && ts <= threshold
                && best.as_ref().is_none_or(|(t, _)| ts > *t)
            {
                best = Some((ts, path));
            }
        }
    }

    let Some((_, path)) = best else {
        return HashMap::new();
    };

    let Ok(json) = load_json(&path) else {
        return HashMap::new();
    };

    let mut map: HashMap<String, HashMap<String, f64>> = HashMap::new();
    if let Some(hotspots) = json.get("hotspots").and_then(|v| v.as_array()) {
        for h in hotspots {
            let scenario = get_str(h, "scenario_name");
            let span = get_str(h, "span_name");
            if let Some(mean) = get_f64(h, "mean_ns") {
                map.entry(scenario).or_default().insert(span, mean);
            }
        }
    }
    map
}

// ─── Formatting ──────────────────────────────────────────────────────────────

fn trend_symbol(trend_pct: Option<f64>) -> String {
    match trend_pct {
        None => "—".to_string(),
        Some(t) if t > 10.0 => format!("📈 +{:.0}%", t),
        Some(t) if t < -10.0 => format!("📉 {:.0}%", t),
        Some(t) => format!("{:+.0}%", t),
    }
}

fn print_digest(output: &HotspotOutput) {
    println!();
    println!("=== Hotspot Digest [run_id: {}] ===", output.run_id);
    println!(
        "Filtering: mean ≥ 10ms · contribution 1–95% of scenario · top {}",
        output.top_n
    );
    println!();
    println!(
        "{:<4}  {:<50}  {:<30}  {:>8}  {:>8}  {:>8}  {:>8}  {:>8}",
        "Rank", "Span", "Scenario", "Mean", "P95", "Contrib%", "7d Trend", "Score"
    );
    println!("{}", "─".repeat(135));

    for h in &output.hotspots {
        println!(
            "{:<4}  {:<50}  {:<30}  {:>8}  {:>8}  {:>7.1}%  {:>9}  {:>8.2}",
            h.rank,
            truncate(&h.span_name, 50),
            truncate(&h.scenario_name, 30),
            format_ns(h.mean_ns),
            format_ns(h.p95_ns),
            h.contribution_pct,
            trend_symbol(h.trend_pct),
            h.hotspot_score,
        );
    }

    println!();
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Walk back from `max` until we land on a UTF-8 character boundary.
    let mut end = max.saturating_sub(1); // room for ellipsis
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

// ─── main ────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();

    let results_json = match load_json(&args.results) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(1);
        }
    };

    let run_id = args
        .run_id
        .unwrap_or_else(|| Utc::now().format("%Y-%m-%dT%H%M%SZ").to_string());

    let week_ago = load_week_ago_means(&args.history_dir);

    let scenarios = results_json
        .get("scenarios")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut candidates: Vec<HotspotEntry> = Vec::new();

    for scenario in &scenarios {
        let scenario_name = get_str(scenario, "name");
        let scenario_mean_ns = get_f64(scenario, "mean_ns").unwrap_or(0.0);
        let scenario_total_ns = get_f64(scenario, "total_duration_ns").unwrap_or(0.0);

        if scenario_total_ns == 0.0 {
            continue;
        }

        let Some(breakdown) = scenario.get("perf_breakdown").and_then(|v| v.as_array()) else {
            continue;
        };

        for span in breakdown {
            let span_name = get_str(span, "name");
            let mean_ns = get_f64(span, "mean_ns").unwrap_or(0.0);
            let total_ns = get_f64(span, "total_ns").unwrap_or(0.0);
            let p95_ns = get_f64(span, "p95_ns").unwrap_or(0.0);
            let call_count = get_u64(span, "call_count");

            // Filter 1: minimum mean duration
            if mean_ns < MIN_MEAN_NS {
                continue;
            }

            // Filter 2+3: contribution range
            let contribution_pct = if scenario_total_ns > 0.0 {
                total_ns / scenario_total_ns * 100.0
            } else {
                0.0
            };

            if !(MIN_CONTRIBUTION_PCT..MAX_CONTRIBUTION_PCT).contains(&contribution_pct) {
                continue;
            }

            let score = hotspot_score(mean_ns, scenario_mean_ns, call_count);

            let trend_pct = week_ago
                .get(scenario_name.as_str())
                .and_then(|spans| spans.get(span_name.as_str()))
                .map(|&week_mean| {
                    if week_mean == 0.0 {
                        0.0
                    } else {
                        (mean_ns - week_mean) / week_mean * 100.0
                    }
                });

            candidates.push(HotspotEntry {
                rank: 0, // filled below
                span_name,
                scenario_name: scenario_name.clone(),
                mean_ns,
                p95_ns,
                total_ns,
                call_count,
                contribution_pct,
                hotspot_score: score,
                trend_pct,
            });
        }
    }

    // Sort descending by score, take top N, assign ranks
    candidates.sort_by(|a, b| {
        b.hotspot_score
            .partial_cmp(&a.hotspot_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.truncate(args.top_n);
    for (i, entry) in candidates.iter_mut().enumerate() {
        entry.rank = i + 1;
    }

    let output = HotspotOutput {
        run_id,
        top_n: args.top_n,
        hotspots: candidates,
    };

    print_digest(&output);

    // Write hotspot JSON to the output path
    match serde_json::to_string_pretty(&output) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&args.output_json, &json) {
                eprintln!(
                    "ERROR: failed to write hotspot JSON to {}: {e}",
                    args.output_json.display()
                );
                std::process::exit(1);
            }
            println!("Hotspot JSON written to {}", args.output_json.display());
        }
        Err(e) => {
            eprintln!("ERROR: failed to serialize hotspot output: {e}");
            std::process::exit(1);
        }
    }
}

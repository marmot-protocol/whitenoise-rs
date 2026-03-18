//! bench_compare — compare two benchmark JSON outputs and report regressions.
//!
//! Usage:
//! ```text
//! bench_compare <baseline.json> <candidate.json>
//!     [--scenario NAME]
//!     [--output-json PATH]
//! ```
//!
//! Exit codes:
//!   0 — all scenarios within thresholds
//!   1 — one or more Warning, no Regression
//!   2 — one or more Regression
//!   3 — one or more BreakingRegression
//!
//! Thresholds are read from the **baseline** JSON so they travel with the baseline.

use std::path::PathBuf;

use clap::Parser;
use serde::Serialize;
use serde_json::Value;

use ::whitenoise::integration_tests::benchmarks::bench_utils::{
    format_ns, get_f64, get_str, get_u64, load_json,
};

#[derive(Parser, Debug)]
#[clap(
    author,
    version,
    about = "Compare two benchmark JSON outputs for regressions"
)]
struct Args {
    /// Path to the baseline JSON file.
    baseline: PathBuf,

    /// Path to the candidate JSON file.
    candidate: PathBuf,

    /// Only compare a specific scenario by name.
    #[clap(long, value_name = "NAME")]
    scenario: Option<String>,

    /// Write machine-readable comparison results to this path.
    #[clap(long, value_name = "PATH")]
    output_json: Option<PathBuf>,
}

// ─── Classification ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
enum Status {
    Improvement,
    MinorImprovement,
    Unchanged,
    Warning,
    Regression,
    BreakingRegression,
}

impl Status {
    fn symbol(&self) -> &'static str {
        match self {
            Status::Improvement => "✅ IMPROVEMENT",
            Status::MinorImprovement => "~ minor improvement",
            Status::Unchanged => "✅",
            Status::Warning => "⚠ WARNING",
            Status::Regression => "✗ REGRESSION",
            Status::BreakingRegression => "✗ BREAKING",
        }
    }
}

fn classify(delta_pct: f64, warn: u32, regress: u32, break_: u32) -> Status {
    if delta_pct >= break_ as f64 {
        Status::BreakingRegression
    } else if delta_pct >= regress as f64 {
        Status::Regression
    } else if delta_pct >= warn as f64 {
        Status::Warning
    } else if delta_pct <= -10.0 {
        Status::Improvement
    } else if delta_pct <= -5.0 {
        Status::MinorImprovement
    } else {
        Status::Unchanged
    }
}

fn delta_pct(baseline: f64, candidate: f64) -> f64 {
    if baseline == 0.0 {
        return 0.0;
    }
    (candidate - baseline) / baseline * 100.0
}

// ─── Output types ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct MetricComparison {
    metric: String,
    baseline: f64,
    candidate: f64,
    delta_pct: f64,
    status: Status,
}

#[derive(Debug, Serialize)]
struct SpanComparison {
    name: String,
    baseline_mean_ns: f64,
    candidate_mean_ns: f64,
    delta_pct: f64,
    status: Status,
}

#[derive(Debug, Serialize)]
struct ScenarioComparison {
    name: String,
    ci_tier: String,
    metrics: Vec<MetricComparison>,
    spans: Vec<SpanComparison>,
    worst_status: Status,
}

#[derive(Debug, Serialize)]
struct ComparisonOutput {
    baseline_git_sha: String,
    candidate_git_sha: String,
    scenarios: Vec<ScenarioComparison>,
    overall_status: Status,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn get_u32(v: &Value, key: &str) -> u32 {
    get_u64(v, key) as u32
}

// ─── Core comparison ─────────────────────────────────────────────────────────

fn compare_scenario(
    baseline: &Value,
    candidate: &Value,
    only: Option<&str>,
) -> Option<ScenarioComparison> {
    let name = get_str(baseline, "name");
    if let Some(filter) = only
        && name != filter
    {
        return None;
    }

    let cand_name = get_str(candidate, "name");
    if name != cand_name {
        return None;
    }

    let thresholds = baseline.get("thresholds")?;
    let warn = get_u32(thresholds, "warn_pct");
    let regress = get_u32(thresholds, "regress_pct");
    let break_ = get_u32(thresholds, "break_pct");
    let ci_tier = get_str(thresholds, "ci_tier");

    let mut metrics = Vec::new();

    // Scalar metrics: mean, p95, p99 (ns, higher = worse)
    for metric in &["mean_ns", "p95_ns", "p99_ns", "total_duration_ns"] {
        let Some(b) = get_f64(baseline, metric) else {
            continue;
        };
        let Some(c) = get_f64(candidate, metric) else {
            continue;
        };
        let d = delta_pct(b, c);
        metrics.push(MetricComparison {
            metric: metric.to_string(),
            baseline: b,
            candidate: c,
            delta_pct: d,
            status: classify(d, warn, regress, break_),
        });
    }

    // Throughput: higher = better, so invert sign for classification
    if let (Some(b), Some(c)) = (
        get_f64(baseline, "throughput_ops_sec"),
        get_f64(candidate, "throughput_ops_sec"),
    ) {
        let d = delta_pct(b, c);
        // Negative delta in throughput means slower — treat as positive regression
        let status = classify(-d, warn, regress, break_);
        metrics.push(MetricComparison {
            metric: "throughput_ops_sec".to_string(),
            baseline: b,
            candidate: c,
            delta_pct: d,
            status,
        });
    }

    // Span breakdowns
    let mut spans = Vec::new();
    if let (Some(b_spans), Some(c_spans)) = (
        baseline.get("perf_breakdown").and_then(|v| v.as_array()),
        candidate.get("perf_breakdown").and_then(|v| v.as_array()),
    ) {
        for b_span in b_spans {
            let span_name = get_str(b_span, "name");
            let Some(c_span) = c_spans.iter().find(|s| get_str(s, "name") == span_name) else {
                continue;
            };
            let Some(b_mean) = get_f64(b_span, "mean_ns") else {
                continue;
            };
            let Some(c_mean) = get_f64(c_span, "mean_ns") else {
                continue;
            };
            let d = delta_pct(b_mean, c_mean);
            spans.push(SpanComparison {
                name: span_name,
                baseline_mean_ns: b_mean,
                candidate_mean_ns: c_mean,
                delta_pct: d,
                status: classify(d, warn, regress, break_),
            });
        }
        // Sort by worst regression first
        spans.sort_by(|a, b| {
            b.delta_pct
                .partial_cmp(&a.delta_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    let worst_status = metrics
        .iter()
        .map(|m| &m.status)
        .chain(spans.iter().map(|s| &s.status))
        .max()
        .cloned()
        .unwrap_or(Status::Unchanged);

    Some(ScenarioComparison {
        name,
        ci_tier,
        metrics,
        spans,
        worst_status,
    })
}

// ─── Human-readable output ───────────────────────────────────────────────────

fn print_comparison(output: &ComparisonOutput) {
    println!();
    println!(
        "Baseline: {}  →  Candidate: {}",
        output.baseline_git_sha, output.candidate_git_sha
    );
    println!();

    for sc in &output.scenarios {
        println!("scenario: {}  [{}]", sc.name, sc.ci_tier);

        for m in &sc.metrics {
            if m.metric == "total_duration_ns" {
                continue; // skip total; mean/p95/p99/throughput are more useful
            }
            let sign = if m.delta_pct >= 0.0 { "+" } else { "" };
            if m.metric == "throughput_ops_sec" {
                println!(
                    "  {:12}  {}{:.1}%  {}  ({:.2} → {:.2} ops/sec)",
                    m.metric,
                    sign,
                    m.delta_pct,
                    m.status.symbol(),
                    m.baseline,
                    m.candidate
                );
            } else {
                println!(
                    "  {:12}  {}{:.1}%  {}  ({} → {})",
                    m.metric,
                    sign,
                    m.delta_pct,
                    m.status.symbol(),
                    format_ns(m.baseline),
                    format_ns(m.candidate)
                );
            }
        }

        let regressed_spans: Vec<_> = sc
            .spans
            .iter()
            .filter(|s| s.status >= Status::Warning)
            .take(5)
            .collect();

        if !regressed_spans.is_empty() {
            println!();
            println!("  Top regressed spans:");
            for s in regressed_spans {
                let sign = if s.delta_pct >= 0.0 { "+" } else { "" };
                println!(
                    "    {:50}  {}{:.1}%  {}  ({} → {})",
                    s.name,
                    sign,
                    s.delta_pct,
                    s.status.symbol(),
                    format_ns(s.baseline_mean_ns),
                    format_ns(s.candidate_mean_ns)
                );
            }
        }

        println!();
    }

    println!(
        "RESULT: {}",
        match output.overall_status {
            Status::BreakingRegression => "BREAKING REGRESSION",
            Status::Regression => "REGRESSION",
            Status::Warning => "WARNING",
            Status::Improvement | Status::MinorImprovement => "IMPROVEMENT",
            Status::Unchanged => "NO REGRESSION",
        }
    );
    println!();
}

// ─── main ────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();

    let baseline_json = match load_json(&args.baseline) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(4);
        }
    };

    let candidate_json = match load_json(&args.candidate) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(4);
        }
    };

    let baseline_scenarios = baseline_json
        .get("scenarios")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let candidate_scenarios = candidate_json
        .get("scenarios")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut comparisons = Vec::new();

    for b_scenario in &baseline_scenarios {
        let b_name = get_str(b_scenario, "name");
        let Some(c_scenario) = candidate_scenarios
            .iter()
            .find(|s| get_str(s, "name") == b_name)
        else {
            eprintln!(
                "WARNING: scenario '{}' present in baseline but not candidate",
                b_name
            );
            continue;
        };

        if let Some(cmp) = compare_scenario(b_scenario, c_scenario, args.scenario.as_deref()) {
            comparisons.push(cmp);
        }
    }

    // Note any new scenarios in the candidate that have no baseline to compare against.
    for c_scenario in &candidate_scenarios {
        let c_name = get_str(c_scenario, "name");
        if !baseline_scenarios
            .iter()
            .any(|s| get_str(s, "name") == c_name)
        {
            eprintln!(
                "NOTE: scenario '{}' is new in candidate — no baseline to compare against",
                c_name
            );
        }
    }

    if comparisons.is_empty() {
        if let Some(ref filter) = args.scenario {
            eprintln!("ERROR: scenario '{}' not found in both files", filter);
        } else {
            eprintln!("WARNING: no matching scenarios found in both files");
        }
        std::process::exit(0);
    }

    let overall_status = comparisons
        .iter()
        .map(|s| &s.worst_status)
        .max()
        .cloned()
        .unwrap_or(Status::Unchanged);

    let output = ComparisonOutput {
        baseline_git_sha: get_str(&baseline_json, "git_sha"),
        candidate_git_sha: get_str(&candidate_json, "git_sha"),
        scenarios: comparisons,
        overall_status,
    };

    print_comparison(&output);

    if let Some(ref out_path) = args.output_json {
        let json = match serde_json::to_string_pretty(&output) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("ERROR: failed to serialize comparison output: {e}");
                std::process::exit(4);
            }
        };
        if let Err(e) = std::fs::write(out_path, json) {
            eprintln!(
                "ERROR: failed to write output JSON to {}: {e}",
                out_path.display()
            );
            std::process::exit(4);
        }
    }

    let exit_code = match output.overall_status {
        Status::BreakingRegression => 3,
        Status::Regression => 2,
        Status::Warning => 1,
        _ => 0,
    };

    std::process::exit(exit_code);
}

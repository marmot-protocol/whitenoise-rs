use serde::Serialize;

use super::benchmark_result::BenchmarkResult;

/// Per-scenario threshold configuration for regression detection.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioThresholds {
    pub warn_pct: u32,
    pub regress_pct: u32,
    pub break_pct: u32,
    pub ci_tier: &'static str,
}

impl Default for ScenarioThresholds {
    fn default() -> Self {
        Self {
            warn_pct: 10,
            regress_pct: 20,
            break_pct: 40,
            ci_tier: "unknown",
        }
    }
}

/// A benchmark result enriched with regression thresholds, ready for JSON output.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioResult {
    #[serde(flatten)]
    pub result: BenchmarkResult,
    pub thresholds: ScenarioThresholds,
}

/// Top-level JSON envelope emitted by `--output-json`.
#[derive(Debug, Clone, Serialize)]
pub struct BenchmarkOutput {
    pub generated_at: String,
    pub git_sha: String,
    pub scenarios: Vec<ScenarioResult>,
}

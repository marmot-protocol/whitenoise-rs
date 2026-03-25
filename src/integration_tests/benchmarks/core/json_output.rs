use std::fmt;

use serde::Serialize;

use super::benchmark_result::BenchmarkResult;

/// CI tier classification for benchmark scenarios.
///
/// Determines how `bench_compare` treats regressions:
/// - `Stable`: merge-gating — regressions block the PR.
/// - `Relay`: informational — regressions are reported but never block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CiTier {
    Stable,
    Relay,
    Unknown,
}

impl fmt::Display for CiTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CiTier::Stable => write!(f, "stable"),
            CiTier::Relay => write!(f, "relay"),
            CiTier::Unknown => write!(f, "unknown"),
        }
    }
}

/// Per-scenario threshold configuration for regression detection.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioThresholds {
    pub warn_pct: u32,
    pub regress_pct: u32,
    pub break_pct: u32,
    pub ci_tier: CiTier,
}

impl Default for ScenarioThresholds {
    fn default() -> Self {
        Self {
            warn_pct: 10,
            regress_pct: 20,
            break_pct: 40,
            ci_tier: CiTier::Unknown,
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

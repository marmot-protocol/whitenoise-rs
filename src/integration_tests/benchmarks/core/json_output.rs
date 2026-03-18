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

/// Look up the thresholds for a scenario by its kebab-case CLI name.
pub fn thresholds_for(scenario_name: &str) -> ScenarioThresholds {
    match scenario_name {
        "messaging-performance" => ScenarioThresholds {
            warn_pct: 5,
            regress_pct: 10,
            break_pct: 20,
            ci_tier: "stable",
        },
        "message-aggregation" => ScenarioThresholds {
            warn_pct: 5,
            regress_pct: 10,
            break_pct: 20,
            ci_tier: "stable",
        },
        "identity-creation" => ScenarioThresholds {
            warn_pct: 5,
            regress_pct: 10,
            break_pct: 20,
            ci_tier: "stable",
        },
        "group-creation" => ScenarioThresholds {
            warn_pct: 10,
            regress_pct: 25,
            break_pct: 40,
            ci_tier: "relay",
        },
        "login-performance" => ScenarioThresholds {
            warn_pct: 10,
            regress_pct: 25,
            break_pct: 40,
            ci_tier: "relay",
        },
        "user-discovery-blocking" => ScenarioThresholds {
            warn_pct: 10,
            regress_pct: 20,
            break_pct: 35,
            ci_tier: "relay",
        },
        "user-discovery-background" => ScenarioThresholds {
            warn_pct: 10,
            regress_pct: 20,
            break_pct: 35,
            ci_tier: "relay",
        },
        "user-search" => ScenarioThresholds {
            warn_pct: 15,
            regress_pct: 30,
            break_pct: 50,
            ci_tier: "relay",
        },
        _ => ScenarioThresholds {
            warn_pct: 10,
            regress_pct: 20,
            break_pct: 40,
            ci_tier: "unknown",
        },
    }
}

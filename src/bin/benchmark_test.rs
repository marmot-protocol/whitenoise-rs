use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use clap::Parser;

use ::whitenoise::init_tracing_with_perf_layer;
use ::whitenoise::integration_tests::benchmarks::chrome_trace::write_perfetto_trace;
use ::whitenoise::integration_tests::benchmarks::core::json_output::{
    BenchmarkOutput, ScenarioResult, thresholds_for,
};
use ::whitenoise::integration_tests::benchmarks::registry::BenchmarkRegistry;
use ::whitenoise::integration_tests::benchmarks::{DETAILED_MODE, init_perf_layer};
use ::whitenoise::*;

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    #[clap(long, value_name = "PATH", required = true)]
    data_dir: PathBuf,

    #[clap(long, value_name = "PATH", required = true)]
    logs_dir: PathBuf,

    /// Only measure initialization timing, then exit without running benchmarks.
    #[clap(long)]
    init_only: bool,

    /// Log in with the given nsec/hex private key, wait for the contact list
    /// to sync from relays, then shut down. Use this to seed a data directory
    /// with a real account before running `--init-only` measurements.
    #[clap(long, value_name = "NSEC")]
    login: Option<String>,

    /// Write machine-readable JSON results to this path.
    #[clap(long, value_name = "PATH")]
    output_json: Option<PathBuf>,

    /// Capture per-iteration span detail. Required for `--chrome-trace`.
    /// Increases memory usage proportionally to iterations × spans per iteration.
    #[clap(long)]
    detailed: bool,

    /// Write a Perfetto-compatible JSON trace to this path. Requires `--detailed`.
    /// Open the output at <https://ui.perfetto.dev> or `chrome://tracing`.
    #[clap(long, value_name = "PATH")]
    chrome_trace: Option<PathBuf>,

    /// Optional scenario name to run a specific benchmark.
    /// If not provided, runs all benchmarks.
    #[clap(value_name = "SCENARIO")]
    scenario: Option<String>,
}

fn git_sha() -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn write_json_output(
    path: &std::path::Path,
    results: &[::whitenoise::integration_tests::benchmarks::BenchmarkResult],
) -> Result<(), WhitenoiseError> {
    let scenarios: Vec<ScenarioResult> = results
        .iter()
        .map(|r| {
            let cli_name = BenchmarkRegistry::cli_name_for(r).unwrap_or("unknown");
            ScenarioResult {
                result: r.clone(),
                thresholds: thresholds_for(cli_name),
            }
        })
        .collect();

    let output = BenchmarkOutput {
        generated_at: chrono::Utc::now().to_rfc3339(),
        git_sha: git_sha(),
        scenarios,
    };

    let json = serde_json::to_string_pretty(&output)?;
    std::fs::write(path, json)?;

    tracing::info!("JSON results written to {}", path.display());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), WhitenoiseError> {
    let args = Args::parse();

    // Initialise the perf layer BEFORE Whitenoise initialises tracing so that
    // the layer is part of the subscriber stack from the very first span.
    let perf_layer = init_perf_layer();
    init_tracing_with_perf_layer(&args.logs_dir, perf_layer);

    if args.detailed {
        DETAILED_MODE.store(true, Ordering::Relaxed);
    }

    tracing::info!("=== Starting Whitenoise Performance Benchmark Suite ===");

    let config = WhitenoiseConfig::new(&args.data_dir, &args.logs_dir, "com.whitenoise.benchmark");
    if let Err(err) = Whitenoise::initialize_whitenoise(config).await {
        tracing::error!("Failed to initialize Whitenoise: {}", err);
        std::process::exit(1);
    }

    if let Some(ref nsec) = args.login {
        let whitenoise = Whitenoise::get_instance()?;
        let account = whitenoise.login(nsec.clone()).await?;
        tracing::info!("Logged in as {}", account.pubkey.to_hex());

        // Wait for the contact list to arrive from relays
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let follows = whitenoise.follows(&account).await?;
            if !follows.is_empty() {
                tracing::info!("Contact list synced: {} follows", follows.len());
                break;
            }
            if Instant::now() > deadline {
                tracing::warn!("Timed out waiting for contact list (30s)");
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        whitenoise.shutdown().await?;
        return Ok(());
    }

    if args.init_only {
        return Ok(());
    }

    let whitenoise = Whitenoise::get_instance()?;

    let results = match args.scenario {
        Some(ref scenario_name) => {
            BenchmarkRegistry::run_scenario(scenario_name, whitenoise).await?
        }
        None => {
            let results = BenchmarkRegistry::run_all_benchmarks(whitenoise).await?;
            tracing::info!("=== All Performance Benchmarks Completed Successfully ===");
            results
        }
    };

    if let Some(ref json_path) = args.output_json {
        write_json_output(json_path, &results)?;
    }

    if let Some(ref trace_path) = args.chrome_trace {
        if !args.detailed {
            tracing::error!("--chrome-trace requires --detailed; skipping trace output");
        } else {
            write_perfetto_trace(trace_path, &results)?;
        }
    }

    Ok(())
}

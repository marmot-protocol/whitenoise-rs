use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;

use ::whitenoise::init_tracing_with_perf_layer;
use ::whitenoise::integration_tests::benchmarks::init_perf_layer;
use ::whitenoise::integration_tests::benchmarks::registry::BenchmarkRegistry;
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

    /// Optional scenario name to run a specific benchmark.
    /// If not provided, runs all benchmarks.
    #[clap(value_name = "SCENARIO")]
    scenario: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), WhitenoiseError> {
    let args = Args::parse();

    // Initialise the perf layer BEFORE Whitenoise initialises tracing so that
    // the layer is part of the subscriber stack from the very first span.
    let perf_layer = init_perf_layer();
    init_tracing_with_perf_layer(&args.logs_dir, perf_layer);

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

    match args.scenario {
        Some(scenario_name) => {
            BenchmarkRegistry::run_scenario(&scenario_name, whitenoise).await?;
        }
        None => {
            BenchmarkRegistry::run_all_benchmarks(whitenoise).await?;
            tracing::info!("=== All Performance Benchmarks Completed Successfully ===");
        }
    }

    Ok(())
}

use std::path::PathBuf;

use clap::Parser;

use ::whitenoise::integration_tests::benchmarks::registry::BenchmarkRegistry;
use ::whitenoise::*;

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    #[clap(long, value_name = "PATH", required = true)]
    data_dir: PathBuf,

    #[clap(long, value_name = "PATH", required = true)]
    logs_dir: PathBuf,

    /// Optional scenario name to run a specific benchmark.
    /// If not provided, runs all benchmarks.
    #[clap(value_name = "SCENARIO")]
    scenario: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), WhitenoiseError> {
    let args = Args::parse();

    tracing::info!("=== Starting Whitenoise Performance Benchmark Suite ===");

    let config = WhitenoiseConfig::new(&args.data_dir, &args.logs_dir, "com.whitenoise.benchmark");
    if let Err(err) = Whitenoise::initialize_whitenoise(config).await {
        tracing::error!("Failed to initialize Whitenoise: {}", err);
        std::process::exit(1);
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

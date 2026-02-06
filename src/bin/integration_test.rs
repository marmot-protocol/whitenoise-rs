use std::path::PathBuf;

use clap::Parser;

use ::whitenoise::integration_tests::registry::ScenarioRegistry;
use ::whitenoise::*;

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    #[clap(long, value_name = "PATH", required = true)]
    data_dir: PathBuf,

    #[clap(long, value_name = "PATH", required = true)]
    logs_dir: PathBuf,

    /// Optional scenario name to run a specific test scenario.
    /// If not provided, runs all scenarios.
    #[clap(value_name = "SCENARIO")]
    scenario: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), WhitenoiseError> {
    let args = Args::parse();

    // Initialize mock keyring store for integration tests
    // This is required for MDK database encryption in test environments
    Whitenoise::initialize_mock_keyring_store();

    tracing::info!("=== Starting Whitenoise Integration Test Suite ===");

    let config = WhitenoiseConfig::new(
        &args.data_dir,
        &args.logs_dir,
        "com.whitenoise.integration-test",
    );
    if let Err(err) = Whitenoise::initialize_whitenoise(config).await {
        tracing::error!("Failed to initialize Whitenoise: {}", err);
        std::process::exit(1);
    }

    let whitenoise = Whitenoise::get_instance()?;

    match args.scenario {
        Some(scenario_name) => {
            ScenarioRegistry::run_scenario(&scenario_name, whitenoise).await?;
        }
        None => {
            ScenarioRegistry::run_all_scenarios(whitenoise).await?;
            tracing::info!("=== All Integration Test Scenarios Completed Successfully ===");
        }
    }

    Ok(())
}

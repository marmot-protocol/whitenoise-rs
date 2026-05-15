use std::path::PathBuf;

use clap::Parser;
use nostr_sdk::RelayUrl;

use ::whitenoise::integration_tests::registry::ScenarioRegistry;
use ::whitenoise::*;

const INTEGRATION_WHITENOISE_DB_KEY_ID: &str = "integration.whitenoise.db.key.v1";

/// Drains tracing-appender workers when `main` returns by any path.
struct FlushTracingOnDrop;

impl Drop for FlushTracingOnDrop {
    fn drop(&mut self) {
        ::whitenoise::shutdown_tracing();
    }
}

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
    let _flush_tracing = FlushTracingOnDrop;

    // Initialize mock keyring store for integration tests
    // This is required for MDK database encryption in test environments
    Whitenoise::initialize_mock_keyring_store();

    tracing::info!("=== Starting Whitenoise Integration Test Suite ===");

    let local_relays = vec![
        RelayUrl::parse("ws://localhost:8080").unwrap(),
        RelayUrl::parse("ws://localhost:7777").unwrap(),
    ];
    let config = WhitenoiseConfig::new(
        &args.data_dir,
        &args.logs_dir,
        "com.whitenoise.integration-test",
    )
    .with_database_key_id(INTEGRATION_WHITENOISE_DB_KEY_ID)
    .with_discovery_relays(local_relays.clone())
    .with_default_account_relays(local_relays);
    let whitenoise = Whitenoise::new(config).await.inspect_err(|err| {
        tracing::error!(target: "whitenoise::integration_test", "Failed to initialize Whitenoise: {}", err);
    })?;

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

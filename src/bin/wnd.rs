use std::path::PathBuf;

use clap::Parser;
use nostr_sdk::RelayUrl;

use whitenoise::cli::config::{Config, KEYRING_SERVICE_ID};
use whitenoise::cli::error::CliError;
use whitenoise::cli::server;
use whitenoise::{Whitenoise, WhitenoiseConfig};

#[derive(Parser, Debug)]
#[clap(name = "wnd", about = "Whitenoise daemon")]
struct Args {
    #[clap(long, value_name = "PATH")]
    data_dir: Option<PathBuf>,

    #[clap(long, value_name = "PATH")]
    logs_dir: Option<PathBuf>,

    /// Comma-separated discovery relay URLs that override the curated default set.
    /// Used to point the daemon at local relays for end-to-end testing.
    #[clap(long, value_name = "URLS", value_delimiter = ',')]
    discovery_relays: Vec<String>,
}

#[tokio::main]
async fn main() -> whitenoise::cli::Result<()> {
    let args = Args::parse();
    let config = Config::resolve(args.data_dir.as_ref(), args.logs_dir.as_ref());

    let mut wn_config =
        WhitenoiseConfig::new(&config.data_dir, &config.logs_dir, KEYRING_SERVICE_ID);
    if !args.discovery_relays.is_empty() {
        let relays = args
            .discovery_relays
            .iter()
            .map(|raw| {
                RelayUrl::parse(raw)
                    .map_err(|e| CliError::msg(format!("invalid --discovery-relays {raw:?}: {e}")))
            })
            .collect::<whitenoise::cli::Result<Vec<_>>>()?;
        wn_config = wn_config.with_discovery_relays(relays);
    }
    Whitenoise::initialize_whitenoise(wn_config).await?;

    server::run(&config).await
}

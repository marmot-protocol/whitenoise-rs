use std::path::PathBuf;

use clap::Parser;
use nostr_sdk::RelayUrl;

use whitenoise::{Whitenoise, WhitenoiseConfig};
use whitenoise_cli::config::{Config, KEYRING_SERVICE_ID};
use whitenoise_cli::error::CliError;
use whitenoise_cli::server;

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

    /// Comma-separated relay URLs adopted as a freshly-created account's
    /// NIP-65, Inbox, and KeyPackage lists when no existing relay-list events
    /// are found on the network. Used to point the daemon at private or local
    /// relays for end-to-end testing and self-hosted deployments.
    #[clap(long, value_name = "URLS", value_delimiter = ',')]
    default_account_relays: Vec<String>,
}

fn parse_relay_urls(values: &[String], flag: &str) -> whitenoise_cli::Result<Vec<RelayUrl>> {
    values
        .iter()
        .map(|raw| {
            RelayUrl::parse(raw).map_err(|e| CliError::msg(format!("invalid {flag} {raw:?}: {e}")))
        })
        .collect()
}

#[tokio::main]
async fn main() -> whitenoise_cli::Result<()> {
    let args = Args::parse();
    let config = Config::resolve(args.data_dir.as_ref(), args.logs_dir.as_ref());

    let mut wn_config =
        WhitenoiseConfig::new(&config.data_dir, &config.logs_dir, KEYRING_SERVICE_ID);
    if !args.discovery_relays.is_empty() {
        wn_config = wn_config.with_discovery_relays(parse_relay_urls(
            &args.discovery_relays,
            "--discovery-relays",
        )?);
    }
    if !args.default_account_relays.is_empty() {
        wn_config = wn_config.with_default_account_relays(parse_relay_urls(
            &args.default_account_relays,
            "--default-account-relays",
        )?);
    }
    let whitenoise = Whitenoise::new(wn_config).await?;

    server::run(&config, whitenoise).await
}

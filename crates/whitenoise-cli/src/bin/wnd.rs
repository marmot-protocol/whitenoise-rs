use std::path::PathBuf;

use clap::Parser;

use whitenoise::{Whitenoise, WhitenoiseConfig};
use whitenoise_cli::config::{Config, KEYRING_SERVICE_ID};
use whitenoise_cli::server;

#[derive(Parser, Debug)]
#[clap(name = "wnd", about = "Whitenoise daemon")]
struct Args {
    #[clap(long, value_name = "PATH")]
    data_dir: Option<PathBuf>,

    #[clap(long, value_name = "PATH")]
    logs_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> whitenoise_cli::Result<()> {
    let args = Args::parse();
    let config = Config::resolve(args.data_dir.as_ref(), args.logs_dir.as_ref());

    let wn_config = WhitenoiseConfig::new(&config.data_dir, &config.logs_dir, KEYRING_SERVICE_ID);
    Whitenoise::initialize_whitenoise(wn_config).await?;

    server::run(&config).await
}

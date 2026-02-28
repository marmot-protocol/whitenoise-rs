use std::path::PathBuf;

use clap::{Parser, Subcommand};

use whitenoise::cli::commands::{accounts::AccountsCmd, daemon::DaemonCmd, identity};
use whitenoise::cli::config::Config;

#[derive(Parser, Debug)]
#[clap(name = "wn", about = "Whitenoise CLI")]
struct Args {
    /// Output as JSON
    #[clap(long, global = true)]
    json: bool,

    /// Path to daemon socket (overrides default)
    #[clap(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,

    #[clap(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Manage the daemon
    #[clap(subcommand)]
    Daemon(DaemonCmd),

    /// Create a new identity
    CreateIdentity,

    /// Log in with an nsec
    Login {
        /// Use a specific relay for publishing relay lists
        #[clap(long, value_name = "URL")]
        relay: Option<String>,
    },

    /// Log out an account
    Logout {
        /// The npub of the account to log out
        pubkey: String,
    },

    /// Show current account(s)
    Whoami,

    /// Export the nsec for an account
    ExportNsec {
        /// The npub of the account
        pubkey: String,
    },

    /// Manage accounts
    #[clap(subcommand)]
    Accounts(AccountsCmd),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = Config::resolve(None, None);
    let socket = args.socket.unwrap_or_else(|| config.socket_path());

    match args.command {
        Cmd::Daemon(cmd) => cmd.run(&config).await,
        Cmd::CreateIdentity => identity::create_identity(&socket, args.json).await,
        Cmd::Login { relay } => identity::login(&socket, args.json, relay).await,
        Cmd::Logout { pubkey } => identity::logout(&socket, &pubkey, args.json).await,
        Cmd::Whoami => identity::whoami(&socket, args.json).await,
        Cmd::ExportNsec { pubkey } => identity::export_nsec(&socket, &pubkey, args.json).await,
        Cmd::Accounts(cmd) => cmd.run(&socket, args.json).await,
    }
}

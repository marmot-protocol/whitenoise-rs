use std::path::Path;

use clap::Subcommand;

use crate::cli::account;
use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::Request;

#[derive(Debug, Subcommand)]
pub enum RelaysCmd {
    /// Show relay connection statuses for an account
    List {
        /// Filter by relay type: nip65, inbox, or key_package
        #[clap(long = "type", value_name = "TYPE")]
        relay_type: Option<String>,
    },

    /// Add a relay to an account's relay list
    Add {
        /// Relay URL (e.g. wss://relay.example.com)
        url: String,

        /// Relay type: nip65, inbox, or key_package
        #[clap(long = "type", value_name = "TYPE")]
        relay_type: String,
    },

    /// Remove a relay from an account's relay list
    Remove {
        /// Relay URL (e.g. wss://relay.example.com)
        url: String,

        /// Relay type: nip65, inbox, or key_package
        #[clap(long = "type", value_name = "TYPE")]
        relay_type: String,
    },
}

impl RelaysCmd {
    pub async fn run(
        self,
        socket: &Path,
        json: bool,
        account_flag: Option<&str>,
    ) -> anyhow::Result<()> {
        match self {
            Self::List { relay_type } => list(socket, json, account_flag, relay_type).await,
            Self::Add { url, relay_type } => add(socket, json, account_flag, url, relay_type).await,
            Self::Remove { url, relay_type } => {
                remove(socket, json, account_flag, url, relay_type).await
            }
        }
    }
}

async fn list(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    relay_type: Option<String>,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::RelaysList {
            account: pubkey,
            relay_type,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn add(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    url: String,
    relay_type: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::RelaysAdd {
            account: pubkey,
            url,
            relay_type,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn remove(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    url: String,
    relay_type: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::RelaysRemove {
            account: pubkey,
            url,
            relay_type,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

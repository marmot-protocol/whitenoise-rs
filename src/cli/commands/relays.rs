use std::path::Path;

use clap::Subcommand;

use crate::cli::account;
use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::Request;

#[derive(Debug, Subcommand)]
pub enum RelaysCmd {
    /// Show relay connection statuses for an account
    List,
}

impl RelaysCmd {
    pub async fn run(
        self,
        socket: &Path,
        json: bool,
        account_flag: Option<&str>,
    ) -> anyhow::Result<()> {
        match self {
            Self::List => list(socket, json, account_flag).await,
        }
    }
}

async fn list(socket: &Path, json: bool, account_flag: Option<&str>) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(socket, &Request::RelaysList { account: pubkey }).await?;
    output::print_and_exit(&resp, json)
}

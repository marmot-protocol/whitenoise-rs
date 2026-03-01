use std::path::Path;

use clap::Subcommand;

use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::Request;

#[derive(Debug, Subcommand)]
pub enum AccountsCmd {
    /// List all logged-in accounts
    List,
}

impl AccountsCmd {
    pub async fn run(self, socket: &Path, json: bool) -> anyhow::Result<()> {
        match self {
            AccountsCmd::List => list(socket, json).await,
        }
    }
}

async fn list(socket: &Path, json: bool) -> anyhow::Result<()> {
    let resp = client::send(socket, &Request::AllAccounts).await?;
    output::print_and_exit(&resp, json)
}

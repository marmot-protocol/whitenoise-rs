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
    pub async fn run(self, socket: &Path, json: bool) -> crate::cli::Result<()> {
        match self {
            Self::List => list(socket, json).await,
        }
    }
}

async fn list(socket: &Path, json: bool) -> crate::cli::Result<()> {
    let resp = client::send(socket, &Request::AllAccounts).await?;
    output::print_and_exit(&resp, json)
}

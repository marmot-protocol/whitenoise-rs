use std::path::Path;

use clap::Subcommand;

use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::Request;

#[derive(Debug, Subcommand)]
pub enum UsersCmd {
    /// Show a user's profile by pubkey
    Show {
        /// The npub or hex pubkey of the user
        pubkey: String,
    },
}

impl UsersCmd {
    pub async fn run(self, socket: &Path, json: bool) -> anyhow::Result<()> {
        match self {
            UsersCmd::Show { pubkey } => show(socket, json, &pubkey).await,
        }
    }
}

async fn show(socket: &Path, json: bool, pubkey: &str) -> anyhow::Result<()> {
    let resp = client::send(
        socket,
        &Request::UsersShow {
            pubkey: pubkey.into(),
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

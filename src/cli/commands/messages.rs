use std::path::Path;

use clap::Subcommand;

use crate::cli::account;
use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::Request;

#[derive(Debug, Subcommand)]
pub enum MessagesCmd {
    /// List messages in a group
    List {
        /// MLS group ID (hex)
        group_id: String,
    },

    /// Send a message to a group
    Send {
        /// MLS group ID (hex)
        group_id: String,

        /// Message text
        message: String,
    },
}

impl MessagesCmd {
    pub async fn run(
        self,
        socket: &Path,
        json: bool,
        account_flag: Option<&str>,
    ) -> anyhow::Result<()> {
        match self {
            MessagesCmd::List { group_id } => list(socket, json, account_flag, group_id).await,
            MessagesCmd::Send { group_id, message } => {
                send(socket, json, account_flag, group_id, message).await
            }
        }
    }
}

async fn list(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::ListMessages {
            account: pubkey,
            group_id,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn send(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
    message: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::SendMessage {
            account: pubkey,
            group_id,
            message,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

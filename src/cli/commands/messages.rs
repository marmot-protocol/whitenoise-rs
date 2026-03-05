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

    /// Subscribe to live messages in a group
    Subscribe {
        /// MLS group ID (hex)
        group_id: String,
    },

    /// React to a message
    React {
        /// MLS group ID (hex)
        group_id: String,

        /// Message event ID to react to
        message_id: String,

        /// Emoji reaction (defaults to "+")
        #[arg(default_value = "+")]
        emoji: String,
    },

    /// Remove your reaction from a message
    Unreact {
        /// MLS group ID (hex)
        group_id: String,

        /// Message event ID to unreact from
        message_id: String,
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
            Self::List { group_id } => list(socket, json, account_flag, group_id).await,
            Self::Send { group_id, message } => {
                send(socket, json, account_flag, group_id, message).await
            }
            Self::Subscribe { group_id } => subscribe(socket, json, account_flag, group_id).await,
            Self::React {
                group_id,
                message_id,
                emoji,
            } => react(socket, json, account_flag, group_id, message_id, emoji).await,
            Self::Unreact {
                group_id,
                message_id,
            } => unreact(socket, json, account_flag, group_id, message_id).await,
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

async fn subscribe(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let req = Request::MessagesSubscribe {
        account: pubkey,
        group_id,
    };
    let mut had_error = false;
    client::stream(socket, &req, |resp| {
        let ok = output::print_stream_response(resp, json);
        if !ok {
            had_error = true;
        }
        ok
    })
    .await?;
    if had_error {
        std::process::exit(1);
    }
    Ok(())
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

async fn react(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
    message_id: String,
    emoji: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::ReactToMessage {
            account: pubkey,
            group_id,
            message_id,
            emoji,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn unreact(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
    message_id: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::UnreactToMessage {
            account: pubkey,
            group_id,
            message_id,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

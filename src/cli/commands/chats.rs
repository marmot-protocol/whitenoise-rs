use std::path::Path;

use clap::Subcommand;

use crate::cli::account;
use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::{MuteDuration, Request};

#[derive(Debug, Subcommand)]
pub enum ChatsCmd {
    /// List all chats with last message preview
    List,

    /// Subscribe to live chat list updates
    Subscribe,

    /// Archive a chat (hide from main list)
    Archive {
        /// MLS group ID (hex)
        group_id: String,
    },

    /// Unarchive a chat (restore to main list)
    Unarchive {
        /// MLS group ID (hex)
        group_id: String,
    },

    /// List archived chats
    ListArchived,

    /// Subscribe to live archived chat list updates
    SubscribeArchived,

    /// Mute a chat (suppress notifications)
    Mute {
        /// MLS group ID (hex)
        group_id: String,
        /// Duration: "1h", "8h", "1d", "1w", or "forever"
        duration: String,
    },

    /// Unmute a chat (restore notifications)
    Unmute {
        /// MLS group ID (hex)
        group_id: String,
    },
}

impl ChatsCmd {
    pub async fn run(
        self,
        socket: &Path,
        json: bool,
        account_flag: Option<&str>,
    ) -> anyhow::Result<()> {
        match self {
            Self::List => list(socket, json, account_flag).await,
            Self::Subscribe => subscribe(socket, json, account_flag).await,
            Self::Archive { group_id } => archive(socket, json, account_flag, &group_id).await,
            Self::Unarchive { group_id } => unarchive(socket, json, account_flag, &group_id).await,
            Self::ListArchived => list_archived(socket, json, account_flag).await,
            Self::SubscribeArchived => subscribe_archived(socket, json, account_flag).await,
            Self::Mute { group_id, duration } => {
                mute(socket, json, account_flag, &group_id, &duration).await
            }
            Self::Unmute { group_id } => unmute(socket, json, account_flag, &group_id).await,
        }
    }
}

async fn list(socket: &Path, json: bool, account_flag: Option<&str>) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(socket, &Request::ChatsList { account: pubkey }).await?;
    output::print_and_exit(&resp, json)
}

async fn subscribe(socket: &Path, json: bool, account_flag: Option<&str>) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let req = Request::ChatsSubscribe { account: pubkey };
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

async fn archive(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: &str,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::ArchiveChat {
            account: pubkey,
            group_id: group_id.to_string(),
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn unarchive(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: &str,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::UnarchiveChat {
            account: pubkey,
            group_id: group_id.to_string(),
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn list_archived(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(socket, &Request::ArchivedChatsList { account: pubkey }).await?;
    output::print_and_exit(&resp, json)
}

async fn subscribe_archived(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let req = Request::ArchivedChatsSubscribe { account: pubkey };
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

async fn mute(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: &str,
    duration: &str,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let duration = duration
        .parse::<MuteDuration>()
        .map_err(|e| anyhow::anyhow!(e))?;
    let resp = client::send(
        socket,
        &Request::MuteChat {
            account: pubkey,
            group_id: group_id.to_string(),
            duration,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn unmute(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: &str,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::UnmuteChat {
            account: pubkey,
            group_id: group_id.to_string(),
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

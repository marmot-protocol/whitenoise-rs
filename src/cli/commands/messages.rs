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

        /// Cursor timestamp: Unix seconds taken from the `created_at` of the oldest
        /// message in the current page. Only messages strictly before this timestamp
        /// (or at the same second with a smaller ID) are returned.
        #[arg(long)]
        before: Option<u64>,

        /// Companion cursor ID: the `id` field of the same oldest message used for
        /// `--before`. Ensures deterministic ordering when multiple messages share
        /// the same second.
        #[arg(long)]
        before_message_id: Option<String>,

        /// Cursor timestamp: Unix seconds taken from the `created_at` of the newest
        /// message in the current page. Only messages strictly after this timestamp
        /// (or at the same second with a larger ID) are returned.
        #[arg(long)]
        after: Option<u64>,

        /// Companion cursor ID: the `id` field of the same newest message used for
        /// `--after`. Ensures deterministic ordering when multiple messages share
        /// the same second.
        #[arg(long)]
        after_message_id: Option<String>,

        /// Maximum number of messages to return (default: 50, max: 200)
        #[arg(long)]
        limit: Option<u32>,
    },

    /// Send a message to a group
    Send {
        /// MLS group ID (hex)
        group_id: String,

        /// Message text
        message: String,

        /// Reply to a specific message (event ID)
        #[arg(long)]
        reply_to: Option<String>,
    },

    /// Delete a message
    Delete {
        /// MLS group ID (hex)
        group_id: String,

        /// Message event ID to delete
        message_id: String,
    },

    /// Retry sending a failed message
    Retry {
        /// MLS group ID (hex)
        group_id: String,

        /// Event ID of the failed message
        event_id: String,
    },

    /// Search messages by content in a group
    Search {
        /// MLS group ID (hex)
        group_id: String,

        /// Search query (forward-order substring matching)
        query: String,

        /// Maximum number of results (default: 50, max: 200)
        #[arg(long)]
        limit: Option<u32>,
    },

    /// Search messages across all groups
    SearchAll {
        /// Search query (forward-order substring matching)
        query: String,

        /// Maximum number of results (default: 50, max: 200)
        #[arg(long)]
        limit: Option<u32>,
    },

    /// Subscribe to live messages in a group
    Subscribe {
        /// MLS group ID (hex)
        group_id: String,

        /// Maximum number of messages in the initial snapshot (default: 50, max: 200).
        /// Older messages can be fetched on demand with the `list` command.
        #[arg(long)]
        limit: Option<u32>,
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
            Self::List {
                group_id,
                before,
                before_message_id,
                after,
                after_message_id,
                limit,
            } => {
                list(
                    socket,
                    json,
                    account_flag,
                    group_id,
                    before,
                    before_message_id,
                    after,
                    after_message_id,
                    limit,
                )
                .await
            }
            Self::Send {
                group_id,
                message,
                reply_to,
            } => send(socket, json, account_flag, group_id, message, reply_to).await,
            Self::Search {
                group_id,
                query,
                limit,
            } => search(socket, json, account_flag, group_id, query, limit).await,
            Self::SearchAll { query, limit } => {
                search_all(socket, json, account_flag, query, limit).await
            }
            Self::Subscribe { group_id, limit } => {
                subscribe(socket, json, account_flag, group_id, limit).await
            }
            Self::React {
                group_id,
                message_id,
                emoji,
            } => react(socket, json, account_flag, group_id, message_id, emoji).await,
            Self::Unreact {
                group_id,
                message_id,
            } => unreact(socket, json, account_flag, group_id, message_id).await,
            Self::Delete {
                group_id,
                message_id,
            } => delete(socket, json, account_flag, group_id, message_id).await,
            Self::Retry { group_id, event_id } => {
                retry(socket, json, account_flag, group_id, event_id).await
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn list(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
    before: Option<u64>,
    before_message_id: Option<String>,
    after: Option<u64>,
    after_message_id: Option<String>,
    limit: Option<u32>,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::ListMessages {
            account: pubkey,
            group_id,
            before,
            before_message_id,
            after,
            after_message_id,
            limit,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn search(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
    query: String,
    limit: Option<u32>,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::SearchMessages {
            account: pubkey,
            group_id,
            query,
            limit,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn search_all(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    query: String,
    limit: Option<u32>,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::SearchAllMessages {
            account: pubkey,
            query,
            limit,
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
    limit: Option<u32>,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let req = Request::MessagesSubscribe {
        account: pubkey,
        group_id,
        limit,
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
    reply_to: Option<String>,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::SendMessage {
            account: pubkey,
            group_id,
            message,
            reply_to,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn delete(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
    message_id: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::DeleteMessage {
            account: pubkey,
            group_id,
            message_id,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn retry(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
    event_id: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::RetryMessage {
            account: pubkey,
            group_id,
            event_id,
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

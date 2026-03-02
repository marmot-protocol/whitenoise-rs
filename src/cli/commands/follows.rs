use std::path::Path;

use clap::Subcommand;

use crate::cli::account;
use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::Request;

#[derive(Debug, Subcommand)]
pub enum FollowsCmd {
    /// List followed users
    List,

    /// Follow a user
    Add {
        /// User pubkey (npub or hex)
        pubkey: String,
    },

    /// Unfollow a user
    Remove {
        /// User pubkey (npub or hex)
        pubkey: String,
    },

    /// Check if you follow a user
    Check {
        /// User pubkey (npub or hex)
        pubkey: String,
    },
}

impl FollowsCmd {
    pub async fn run(
        self,
        socket: &Path,
        json: bool,
        account_flag: Option<&str>,
    ) -> anyhow::Result<()> {
        match self {
            FollowsCmd::List => list(socket, json, account_flag).await,
            FollowsCmd::Add { pubkey } => add(socket, json, account_flag, pubkey).await,
            FollowsCmd::Remove { pubkey } => remove(socket, json, account_flag, pubkey).await,
            FollowsCmd::Check { pubkey } => check(socket, json, account_flag, pubkey).await,
        }
    }
}

async fn list(socket: &Path, json: bool, account_flag: Option<&str>) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(socket, &Request::FollowsList { account: pubkey }).await?;
    output::print_and_exit(&resp, json)
}

async fn add(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    pubkey: String,
) -> anyhow::Result<()> {
    let account = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(socket, &Request::FollowsAdd { account, pubkey }).await?;
    output::print_and_exit(&resp, json)
}

async fn remove(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    pubkey: String,
) -> anyhow::Result<()> {
    let account = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(socket, &Request::FollowsRemove { account, pubkey }).await?;
    output::print_and_exit(&resp, json)
}

async fn check(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    pubkey: String,
) -> anyhow::Result<()> {
    let account = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(socket, &Request::FollowsCheck { account, pubkey }).await?;
    output::print_and_exit(&resp, json)
}

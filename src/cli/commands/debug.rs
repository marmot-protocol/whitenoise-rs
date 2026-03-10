use std::path::Path;

use clap::Subcommand;

use crate::cli::account;
use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::Request;

#[derive(Debug, Subcommand)]
pub enum DebugCmd {
    /// Dump the current relay-control snapshot
    RelayControlState,

    /// Show subscription health status
    Health,

    /// Show MLS ratchet tree info for a group
    RatchetTree {
        /// MLS group ID (hex)
        group_id: String,
    },
}

impl DebugCmd {
    pub async fn run(
        self,
        socket: &Path,
        json: bool,
        account_flag: Option<&str>,
    ) -> anyhow::Result<()> {
        match self {
            Self::RelayControlState => relay_control_state(socket, json).await,
            Self::Health => health(socket, json, account_flag).await,
            Self::RatchetTree { group_id } => {
                ratchet_tree(socket, json, account_flag, group_id).await
            }
        }
    }
}

async fn relay_control_state(socket: &Path, json: bool) -> anyhow::Result<()> {
    let resp = client::send(socket, &Request::DebugRelayControlState).await?;
    output::print_and_exit(&resp, json)
}

async fn health(socket: &Path, json: bool, account_flag: Option<&str>) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(socket, &Request::DebugHealth { account: pubkey }).await?;
    output::print_and_exit(&resp, json)
}

async fn ratchet_tree(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::DebugRatchetTree {
            account: pubkey,
            group_id,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

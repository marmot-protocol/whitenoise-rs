use std::path::Path;

use clap::Subcommand;

use crate::cli::account;
use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::Request;

#[derive(Debug, Subcommand)]
pub enum GroupsCmd {
    /// List visible groups
    List,

    /// Create a new group
    Create {
        /// Group name
        name: String,

        /// Member pubkeys (npub or hex) to invite
        #[clap(value_name = "MEMBER")]
        members: Vec<String>,

        /// Group description
        #[clap(long)]
        description: Option<String>,
    },

    /// Show group details
    Show {
        /// MLS group ID (hex)
        group_id: String,
    },

    /// Add members to a group
    AddMembers {
        /// MLS group ID (hex)
        group_id: String,

        /// Member pubkeys (npub or hex) to add
        #[clap(required = true, value_name = "MEMBER")]
        members: Vec<String>,
    },
}

impl GroupsCmd {
    pub async fn run(
        self,
        socket: &Path,
        json: bool,
        account_flag: Option<&str>,
    ) -> anyhow::Result<()> {
        match self {
            GroupsCmd::List => list(socket, json, account_flag).await,
            GroupsCmd::Create {
                name,
                members,
                description,
            } => create(socket, json, account_flag, name, members, description).await,
            GroupsCmd::Show { group_id } => show(socket, json, account_flag, group_id).await,
            GroupsCmd::AddMembers { group_id, members } => {
                add_members(socket, json, account_flag, group_id, members).await
            }
        }
    }
}

async fn create(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    name: String,
    members: Vec<String>,
    description: Option<String>,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::CreateGroup {
            account: pubkey,
            name,
            description,
            members,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn show(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::GetGroup {
            account: pubkey,
            group_id,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn add_members(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
    members: Vec<String>,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::AddMembers {
            account: pubkey,
            group_id,
            members,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn list(socket: &Path, json: bool, account_flag: Option<&str>) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(socket, &Request::VisibleGroups { account: pubkey }).await?;
    output::print_and_exit(&resp, json)
}

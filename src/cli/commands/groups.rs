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

    /// Remove members from a group
    RemoveMembers {
        /// MLS group ID (hex)
        group_id: String,

        /// Member pubkeys (npub or hex) to remove
        #[clap(required = true, value_name = "MEMBER")]
        members: Vec<String>,
    },

    /// List group members
    Members {
        /// MLS group ID (hex)
        group_id: String,
    },

    /// List group admins
    Admins {
        /// MLS group ID (hex)
        group_id: String,
    },

    /// List group relays
    Relays {
        /// MLS group ID (hex)
        group_id: String,
    },

    /// Leave a group
    Leave {
        /// MLS group ID (hex)
        group_id: String,
    },

    /// Rename a group
    Rename {
        /// MLS group ID (hex)
        group_id: String,

        /// New group name
        name: String,
    },

    /// List pending group invites
    Invites,

    /// Accept a group invite
    Accept {
        /// MLS group ID (hex)
        group_id: String,
    },

    /// Decline a group invite
    Decline {
        /// MLS group ID (hex)
        group_id: String,
    },

    /// Promote a member to admin
    Promote {
        /// MLS group ID (hex)
        group_id: String,

        /// Member pubkey (npub or hex) to promote
        pubkey: String,
    },

    /// Demote an admin to regular member
    Demote {
        /// MLS group ID (hex)
        group_id: String,

        /// Admin pubkey (npub or hex) to demote
        pubkey: String,
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
            Self::List => list(socket, json, account_flag).await,
            Self::Create {
                name,
                members,
                description,
            } => create(socket, json, account_flag, name, members, description).await,
            Self::Show { group_id } => show(socket, json, account_flag, group_id).await,
            Self::AddMembers { group_id, members } => {
                add_members(socket, json, account_flag, group_id, members).await
            }
            Self::RemoveMembers { group_id, members } => {
                remove_members(socket, json, account_flag, group_id, members).await
            }
            Self::Members { group_id } => members(socket, json, account_flag, group_id).await,
            Self::Admins { group_id } => admins(socket, json, account_flag, group_id).await,
            Self::Relays { group_id } => relays(socket, json, account_flag, group_id).await,
            Self::Leave { group_id } => leave(socket, json, account_flag, group_id).await,
            Self::Rename { group_id, name } => {
                rename(socket, json, account_flag, group_id, name).await
            }
            Self::Invites => invites(socket, json, account_flag).await,
            Self::Accept { group_id } => accept(socket, json, account_flag, group_id).await,
            Self::Decline { group_id } => decline(socket, json, account_flag, group_id).await,
            Self::Promote { group_id, pubkey } => {
                promote(socket, json, account_flag, group_id, pubkey).await
            }
            Self::Demote { group_id, pubkey } => {
                demote(socket, json, account_flag, group_id, pubkey).await
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

async fn remove_members(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
    members: Vec<String>,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::RemoveMembers {
            account: pubkey,
            group_id,
            members,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn members(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::GroupMembers {
            account: pubkey,
            group_id,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn admins(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::GroupAdmins {
            account: pubkey,
            group_id,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn relays(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::GroupRelays {
            account: pubkey,
            group_id,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn leave(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::LeaveGroup {
            account: pubkey,
            group_id,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn rename(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
    name: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::RenameGroup {
            account: pubkey,
            group_id,
            name,
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

async fn invites(socket: &Path, json: bool, account_flag: Option<&str>) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(socket, &Request::GroupInvites { account: pubkey }).await?;
    output::print_and_exit(&resp, json)
}

async fn accept(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::AcceptInvite {
            account: pubkey,
            group_id,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn decline(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::DeclineInvite {
            account: pubkey,
            group_id,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn promote(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
    pubkey: String,
) -> anyhow::Result<()> {
    let account = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::GroupPromote {
            account,
            group_id,
            pubkey,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn demote(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
    pubkey: String,
) -> anyhow::Result<()> {
    let account = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::GroupDemote {
            account,
            group_id,
            pubkey,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

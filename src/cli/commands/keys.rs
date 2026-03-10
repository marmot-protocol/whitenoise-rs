use std::path::Path;

use clap::Subcommand;

use crate::cli::account;
use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::Request;

#[derive(Debug, Subcommand)]
pub enum KeysCmd {
    /// List key packages published on relays
    List,

    /// Publish a new key package
    Publish,

    /// Delete a specific key package by event ID
    Delete {
        /// Event ID of the key package to delete
        event_id: String,
    },

    /// Delete all key packages from relays
    DeleteAll {
        /// Required to confirm destructive operation
        #[clap(long)]
        confirm: bool,
    },

    /// Check if a user has a valid key package (creates a local user record if not already present)
    Check {
        /// User pubkey (npub or hex)
        pubkey: String,
    },
}

impl KeysCmd {
    pub async fn run(
        self,
        socket: &Path,
        json: bool,
        account_flag: Option<&str>,
    ) -> anyhow::Result<()> {
        match self {
            Self::List => list(socket, json, account_flag).await,
            Self::Publish => publish(socket, json, account_flag).await,
            Self::Delete { event_id } => delete(socket, json, account_flag, event_id).await,
            Self::DeleteAll { confirm } => {
                if !confirm {
                    anyhow::bail!(
                        "this will delete ALL published key packages. Pass --confirm to proceed."
                    );
                }
                delete_all(socket, json, account_flag).await
            }
            Self::Check { pubkey } => check(socket, json, &pubkey).await,
        }
    }
}

async fn list(socket: &Path, json: bool, account_flag: Option<&str>) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(socket, &Request::KeysList { account: pubkey }).await?;
    output::print_and_exit(&resp, json)
}

async fn publish(socket: &Path, json: bool, account_flag: Option<&str>) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(socket, &Request::KeysPublish { account: pubkey }).await?;
    output::print_and_exit(&resp, json)
}

async fn delete(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    event_id: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::KeysDelete {
            account: pubkey,
            event_id,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn delete_all(socket: &Path, json: bool, account_flag: Option<&str>) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(socket, &Request::KeysDeleteAll { account: pubkey }).await?;
    output::print_and_exit(&resp, json)
}

async fn check(socket: &Path, json: bool, pubkey: &str) -> anyhow::Result<()> {
    let resp = client::send(
        socket,
        &Request::KeysCheck {
            pubkey: pubkey.to_string(),
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

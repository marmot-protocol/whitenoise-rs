use std::path::Path;

use clap::Subcommand;

use crate::cli::account;
use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::Request;

#[derive(Debug, Subcommand)]
pub enum ProfileCmd {
    /// Show account profile metadata
    Show,

    /// Update account profile metadata
    Update {
        /// Username (NIP-01 name)
        #[clap(long)]
        name: Option<String>,

        /// Display name
        #[clap(long)]
        display_name: Option<String>,

        /// About / bio
        #[clap(long)]
        about: Option<String>,

        /// Profile picture URL
        #[clap(long)]
        picture: Option<String>,

        /// NIP-05 identifier (e.g. user@domain.com)
        #[clap(long)]
        nip05: Option<String>,

        /// Lightning address (e.g. user@getalby.com)
        #[clap(long)]
        lud16: Option<String>,
    },
}

impl ProfileCmd {
    pub async fn run(
        self,
        socket: &Path,
        json: bool,
        account_flag: Option<&str>,
    ) -> anyhow::Result<()> {
        match self {
            Self::Show => show(socket, json, account_flag).await,
            Self::Update {
                name,
                display_name,
                about,
                picture,
                nip05,
                lud16,
            } => {
                update(
                    socket,
                    json,
                    account_flag,
                    name,
                    display_name,
                    about,
                    picture,
                    nip05,
                    lud16,
                )
                .await
            }
        }
    }
}

async fn show(socket: &Path, json: bool, account_flag: Option<&str>) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(socket, &Request::ProfileShow { account: pubkey }).await?;
    output::print_and_exit(&resp, json)
}

#[allow(clippy::too_many_arguments)]
async fn update(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    name: Option<String>,
    display_name: Option<String>,
    about: Option<String>,
    picture: Option<String>,
    nip05: Option<String>,
    lud16: Option<String>,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::ProfileUpdate {
            account: pubkey,
            name,
            display_name,
            about,
            picture,
            nip05,
            lud16,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

use std::path::Path;

use clap::Subcommand;

use crate::cli::account;
use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::Request;

#[derive(Debug, Subcommand)]
pub enum MediaCmd {
    /// Upload a file to a group chat
    Upload {
        /// MLS group ID (hex)
        group_id: String,

        /// Path to the file to upload
        file_path: String,
    },

    /// Download a media file from a group chat
    Download {
        /// MLS group ID (hex)
        group_id: String,

        /// Original file hash (hex, SHA-256)
        file_hash: String,
    },

    /// List media files in a group chat
    List {
        /// MLS group ID (hex)
        group_id: String,
    },
}

impl MediaCmd {
    pub async fn run(
        self,
        socket: &Path,
        json: bool,
        account_flag: Option<&str>,
    ) -> anyhow::Result<()> {
        match self {
            Self::Upload {
                group_id,
                file_path,
            } => upload(socket, json, account_flag, group_id, file_path).await,
            Self::Download {
                group_id,
                file_hash,
            } => download(socket, json, account_flag, group_id, file_hash).await,
            Self::List { group_id } => list(socket, json, group_id).await,
        }
    }
}

async fn upload(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
    file_path: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::UploadMedia {
            account: pubkey,
            group_id,
            file_path,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn list(socket: &Path, json: bool, group_id: String) -> anyhow::Result<()> {
    let resp = client::send(socket, &Request::ListMedia { group_id }).await?;
    output::print_and_exit(&resp, json)
}

async fn download(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    group_id: String,
    file_hash: String,
) -> anyhow::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let resp = client::send(
        socket,
        &Request::DownloadMedia {
            account: pubkey,
            group_id,
            file_hash,
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

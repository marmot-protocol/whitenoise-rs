use std::path::Path;

use clap::Subcommand;

use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::Request;

#[derive(Debug, Subcommand)]
pub enum NotificationsCmd {
    /// Subscribe to live notifications
    Subscribe,
}

impl NotificationsCmd {
    pub async fn run(self, socket: &Path, json: bool) -> anyhow::Result<()> {
        match self {
            Self::Subscribe => subscribe(socket, json).await,
        }
    }
}

async fn subscribe(socket: &Path, json: bool) -> anyhow::Result<()> {
    let req = Request::NotificationsSubscribe;
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

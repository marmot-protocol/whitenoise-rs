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
            NotificationsCmd::Subscribe => subscribe(socket, json).await,
        }
    }
}

async fn subscribe(socket: &Path, json: bool) -> anyhow::Result<()> {
    let req = Request::NotificationsSubscribe;
    client::stream(socket, &req, |resp| {
        output::print_response(resp, json);
        true
    })
    .await
}

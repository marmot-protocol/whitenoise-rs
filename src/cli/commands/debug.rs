use std::path::Path;

use clap::Subcommand;

use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::Request;

#[derive(Debug, Subcommand)]
pub enum DebugCmd {
    /// Dump the current relay-control snapshot
    RelayControlState,
}

impl DebugCmd {
    pub async fn run(self, socket: &Path, json: bool) -> anyhow::Result<()> {
        match self {
            Self::RelayControlState => relay_control_state(socket, json).await,
        }
    }
}

async fn relay_control_state(socket: &Path, json: bool) -> anyhow::Result<()> {
    let resp = client::send(socket, &Request::DebugRelayControlState).await?;
    output::print_and_exit(&resp, json)
}

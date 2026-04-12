use std::process::Command;

use clap::Subcommand;

use crate::cli::config::Config;
use crate::cli::error::CliError;
use crate::cli::server;

#[derive(Debug, Subcommand)]
pub enum DaemonCmd {
    /// Start the daemon (runs in the foreground)
    Start,
    /// Stop the running daemon
    Stop,
    /// Check if the daemon is running
    Status,
}

impl DaemonCmd {
    pub async fn run(self, config: &Config) -> crate::cli::Result<()> {
        match self {
            Self::Start => start(config).await,
            Self::Stop => server::stop_daemon(config),
            Self::Status => status(config),
        }
    }
}

async fn start(config: &Config) -> crate::cli::Result<()> {
    // When invoked via `wn daemon start`, spawn `wnd` as a detached child
    // and return immediately. When invoked directly as `wnd`, this path
    // isn't used — wnd.rs calls server::run() directly.
    let wnd = which_wnd()?;
    let mut cmd = Command::new(wnd);
    cmd.arg("--data-dir").arg(&config.data_dir);
    cmd.arg("--logs-dir").arg(&config.logs_dir);
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    cmd.spawn()
        .map_err(|e| CliError::msg(format!("failed to start daemon: {e}")))?;
    println!("daemon started");
    Ok(())
}

fn status(config: &Config) -> crate::cli::Result<()> {
    match server::is_daemon_running(config) {
        Some(pid) => {
            println!("daemon running (pid {pid})");
            println!("socket: {}", config.socket_path().display());
        }
        None => {
            println!("daemon not running");
        }
    }
    Ok(())
}

fn which_wnd() -> crate::cli::Result<std::path::PathBuf> {
    // Look next to the current executable first (cargo install puts both binaries together)
    if let Ok(current) = std::env::current_exe() {
        let sibling = current.parent().unwrap_or(current.as_ref()).join("wnd");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    // Fall back to PATH
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths).find_map(|dir| {
                let candidate = dir.join("wnd");
                candidate.is_file().then_some(candidate)
            })
        })
        .ok_or_else(|| CliError::msg("wnd not found. Ensure it's installed and on your PATH."))
}

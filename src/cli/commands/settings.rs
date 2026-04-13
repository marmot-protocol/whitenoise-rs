use std::path::Path;

use clap::Subcommand;

use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::Request;

#[derive(Debug, Subcommand)]
pub enum SettingsCmd {
    /// Show current settings
    Show,

    /// Set the theme mode
    Theme {
        /// Theme mode: light, dark, or system
        mode: String,
    },

    /// Set the language
    Language {
        /// Language: system, en, es, fr, de, it, pt, ru, tr
        lang: String,
    },
}

impl SettingsCmd {
    pub async fn run(self, socket: &Path, json: bool) -> crate::cli::Result<()> {
        match self {
            Self::Show => show(socket, json).await,
            Self::Theme { mode } => theme(socket, json, &mode).await,
            Self::Language { lang } => language(socket, json, &lang).await,
        }
    }
}

async fn show(socket: &Path, json: bool) -> crate::cli::Result<()> {
    let resp = client::send(socket, &Request::SettingsShow).await?;
    output::print_and_exit(&resp, json)
}

async fn theme(socket: &Path, json: bool, mode: &str) -> crate::cli::Result<()> {
    let resp = client::send(socket, &Request::SettingsTheme { theme: mode.into() }).await?;
    output::print_and_exit(&resp, json)
}

async fn language(socket: &Path, json: bool, lang: &str) -> crate::cli::Result<()> {
    let resp = client::send(
        socket,
        &Request::SettingsLanguage {
            language: lang.into(),
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

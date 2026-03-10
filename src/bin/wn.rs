use std::path::PathBuf;

use clap::{Parser, Subcommand};

use whitenoise::cli::commands::{
    accounts::AccountsCmd, chats::ChatsCmd, daemon::DaemonCmd, debug::DebugCmd,
    follows::FollowsCmd, groups::GroupsCmd, identity, keys::KeysCmd, media::MediaCmd,
    messages::MessagesCmd, notifications::NotificationsCmd, profile::ProfileCmd, relays::RelaysCmd,
    settings::SettingsCmd, users::UsersCmd,
};
use whitenoise::cli::config::Config;
use whitenoise::cli::protocol::Request;
use whitenoise::cli::{client, output};

#[derive(Parser, Debug)]
#[clap(name = "wn", about = "Whitenoise CLI", version)]
struct Args {
    /// Output as JSON
    #[clap(long, global = true)]
    json: bool,

    /// Path to daemon socket (overrides default)
    #[clap(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// Account to use (npub or hex pubkey)
    #[clap(long, global = true, value_name = "NPUB")]
    account: Option<String>,

    #[clap(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Manage the daemon
    #[clap(subcommand)]
    Daemon(DaemonCmd),

    /// Development and troubleshooting commands
    #[clap(subcommand)]
    Debug(DebugCmd),

    /// Create a new identity
    CreateIdentity,

    /// Log in with an nsec
    Login {
        /// Use a specific relay for publishing relay lists
        #[clap(long, value_name = "URL")]
        relay: Option<String>,
    },

    /// Log out an account
    Logout {
        /// The npub of the account to log out
        pubkey: String,
    },

    /// Show current account(s)
    Whoami,

    /// Export the nsec for an account
    ExportNsec {
        /// The npub of the account
        pubkey: String,
    },

    /// Manage accounts
    #[clap(subcommand)]
    Accounts(AccountsCmd),

    /// Manage chats
    #[clap(subcommand)]
    Chats(ChatsCmd),

    /// Manage groups
    #[clap(subcommand)]
    Groups(GroupsCmd),

    /// Manage media files
    #[clap(subcommand)]
    Media(MediaCmd),

    /// Manage messages
    #[clap(subcommand)]
    Messages(MessagesCmd),

    /// Manage follows
    #[clap(subcommand)]
    Follows(FollowsCmd),

    /// Manage profile metadata
    #[clap(subcommand)]
    Profile(ProfileCmd),

    /// Show relay statuses
    #[clap(subcommand)]
    Relays(RelaysCmd),

    /// Manage app settings
    #[clap(subcommand)]
    Settings(SettingsCmd),

    /// Look up users
    #[clap(subcommand)]
    Users(UsersCmd),

    /// Subscribe to notifications
    #[clap(subcommand)]
    Notifications(NotificationsCmd),

    /// Manage MLS key packages
    #[clap(subcommand)]
    Keys(KeysCmd),

    /// Delete all data (database, MLS state, logs). Daemon must be restarted after this command.
    Reset {
        /// Required to confirm destructive operation
        #[clap(long)]
        confirm: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = Config::resolve(None, None);
    let socket = args.socket.unwrap_or_else(|| config.socket_path());

    match args.command {
        Cmd::Daemon(cmd) => cmd.run(&config).await,
        Cmd::Debug(cmd) => cmd.run(&socket, args.json, args.account.as_deref()).await,
        Cmd::CreateIdentity => identity::create_identity(&socket, args.json).await,
        Cmd::Login { relay } => identity::login(&socket, args.json, relay).await,
        Cmd::Logout { pubkey } => identity::logout(&socket, &pubkey, args.json).await,
        Cmd::Whoami => identity::whoami(&socket, args.json).await,
        Cmd::ExportNsec { pubkey } => identity::export_nsec(&socket, &pubkey, args.json).await,
        Cmd::Accounts(cmd) => cmd.run(&socket, args.json).await,
        Cmd::Chats(cmd) => cmd.run(&socket, args.json, args.account.as_deref()).await,
        Cmd::Groups(cmd) => cmd.run(&socket, args.json, args.account.as_deref()).await,
        Cmd::Media(cmd) => cmd.run(&socket, args.json, args.account.as_deref()).await,
        Cmd::Messages(cmd) => cmd.run(&socket, args.json, args.account.as_deref()).await,
        Cmd::Follows(cmd) => cmd.run(&socket, args.json, args.account.as_deref()).await,
        Cmd::Profile(cmd) => cmd.run(&socket, args.json, args.account.as_deref()).await,
        Cmd::Relays(cmd) => cmd.run(&socket, args.json, args.account.as_deref()).await,
        Cmd::Settings(cmd) => cmd.run(&socket, args.json).await,
        Cmd::Users(cmd) => cmd.run(&socket, args.json, args.account.as_deref()).await,
        Cmd::Notifications(cmd) => cmd.run(&socket, args.json).await,
        Cmd::Keys(cmd) => cmd.run(&socket, args.json, args.account.as_deref()).await,
        Cmd::Reset { confirm } => {
            if !confirm {
                anyhow::bail!(
                    "this will delete ALL data (database, MLS state, logs). Pass --confirm to proceed."
                );
            }
            let resp = client::send(&socket, &Request::DeleteAllData).await?;
            output::print_and_exit(&resp, args.json)
        }
    }
}

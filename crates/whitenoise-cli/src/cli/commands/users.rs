use std::path::Path;

use clap::Subcommand;

use crate::cli::account;
use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::Request;

#[derive(Debug, Subcommand)]
pub enum UsersCmd {
    /// Show a user's profile by pubkey
    Show {
        /// The npub or hex pubkey of the user
        pubkey: String,
    },

    /// Search for users by name, username, or description
    Search {
        /// Search query
        query: String,

        /// Social radius range (e.g. 0..2, 0..4)
        #[clap(long, default_value = "0..2", value_parser = parse_radius)]
        radius: (u8, u8),
    },
}

impl UsersCmd {
    pub async fn run(
        self,
        socket: &Path,
        json: bool,
        account_flag: Option<&str>,
    ) -> crate::cli::Result<()> {
        match self {
            Self::Show { pubkey } => show(socket, json, &pubkey).await,
            Self::Search { query, radius } => {
                search(socket, json, account_flag, &query, radius.0, radius.1).await
            }
        }
    }
}

fn parse_radius(s: &str) -> Result<(u8, u8), String> {
    let parts: Vec<&str> = s.split("..").collect();
    if parts.len() != 2 {
        return Err("expected format: START..END (e.g. 0..2)".to_string());
    }
    let start: u8 = parts[0]
        .parse()
        .map_err(|_| format!("invalid radius start: {}", parts[0]))?;
    let end: u8 = parts[1]
        .parse()
        .map_err(|_| format!("invalid radius end: {}", parts[1]))?;
    if start > end {
        return Err(format!("radius start ({start}) must be <= end ({end})"));
    }
    Ok((start, end))
}

async fn show(socket: &Path, json: bool, pubkey: &str) -> crate::cli::Result<()> {
    let resp = client::send(
        socket,
        &Request::UsersShow {
            pubkey: pubkey.into(),
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

async fn search(
    socket: &Path,
    json: bool,
    account_flag: Option<&str>,
    query: &str,
    radius_start: u8,
    radius_end: u8,
) -> crate::cli::Result<()> {
    let pubkey = account::resolve_account(socket, account_flag).await?;
    let req = Request::UsersSearch {
        account: pubkey,
        query: query.to_string(),
        radius_start,
        radius_end,
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_radius_valid() {
        assert_eq!(parse_radius("0..2"), Ok((0, 2)));
        assert_eq!(parse_radius("0..0"), Ok((0, 0)));
        assert_eq!(parse_radius("3..5"), Ok((3, 5)));
    }

    #[test]
    fn parse_radius_start_greater_than_end() {
        let err = parse_radius("3..1").unwrap_err();
        assert!(err.contains("start (3) must be <= end (1)"));
    }

    #[test]
    fn parse_radius_missing_separator() {
        let err = parse_radius("5").unwrap_err();
        assert!(err.contains("expected format"));
    }

    #[test]
    fn parse_radius_non_numeric() {
        assert!(
            parse_radius("a..2")
                .unwrap_err()
                .contains("invalid radius start")
        );
        assert!(
            parse_radius("0..b")
                .unwrap_err()
                .contains("invalid radius end")
        );
    }

    #[test]
    fn parse_radius_overflow() {
        assert!(parse_radius("0..256").is_err());
    }

    #[test]
    fn parse_radius_empty_input() {
        assert!(parse_radius("").is_err());
    }
}

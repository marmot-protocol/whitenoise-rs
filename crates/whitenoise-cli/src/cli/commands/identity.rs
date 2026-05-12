use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use crate::cli::client;
use crate::cli::output;
use crate::cli::protocol::{Request, Response};

pub async fn create_identity(socket: &Path, json: bool) -> crate::cli::Result<()> {
    let resp = client::send(socket, &Request::CreateIdentity).await?;
    output::print_and_exit(&resp, json)
}

pub async fn login(socket: &Path, json: bool, relay: Option<String>) -> crate::cli::Result<()> {
    let nsec = read_nsec()?;
    let resp = client::send(socket, &Request::LoginStart { nsec }).await?;

    if resp.error.is_some() {
        return output::print_and_exit(&resp, json);
    }

    // Check if login needs relay resolution
    let status = resp
        .result
        .as_ref()
        .and_then(|v| v.get("status"))
        .and_then(|v| v.as_str());

    if status == Some("Complete") {
        if json {
            output::print_response(&resp, true);
        } else {
            print_login_success(&resp);
        }
        return Ok(());
    }

    // NeedsRelayLists — resolve relay configuration
    let pubkey = resp
        .result
        .as_ref()
        .and_then(|v| v.get("account"))
        .and_then(|v| v.get("pubkey"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            crate::cli::error::CliError::msg("unexpected login response: missing pubkey")
        })?
        .to_string();

    let relay_resp = match relay {
        Some(url) => {
            client::send(
                socket,
                &Request::LoginWithCustomRelay {
                    pubkey: pubkey.clone(),
                    relay_url: url,
                },
            )
            .await?
        }
        None => {
            eprintln!("Relay lists not found on the network.");
            eprint!("Use default relays? [Y/n] ");
            io::stderr().flush()?;

            let mut answer = String::new();
            io::stdin().lock().read_line(&mut answer)?;
            let answer = answer.trim();

            if answer.is_empty()
                || answer.eq_ignore_ascii_case("y")
                || answer.eq_ignore_ascii_case("yes")
            {
                client::send(
                    socket,
                    &Request::LoginPublishDefaultRelays {
                        pubkey: pubkey.clone(),
                    },
                )
                .await?
            } else {
                eprint!("Relay URL: ");
                io::stderr().flush()?;

                let mut url = String::new();
                io::stdin().lock().read_line(&mut url)?;
                let url = url.trim().to_string();

                if url.is_empty() {
                    // User bailed — cancel the pending login
                    let _ = client::send(socket, &Request::LoginCancel { pubkey }).await;
                    return Err(crate::cli::error::CliError::msg("login cancelled"));
                }

                client::send(
                    socket,
                    &Request::LoginWithCustomRelay {
                        pubkey: pubkey.clone(),
                        relay_url: url,
                    },
                )
                .await?
            }
        }
    };

    if json {
        output::print_response(&relay_resp, true);
    } else if relay_resp.error.is_some() {
        output::print_response(&relay_resp, false);
    } else {
        print_login_success(&relay_resp);
    }
    if relay_resp.error.is_some() {
        std::process::exit(1);
    }
    Ok(())
}

pub async fn logout(socket: &Path, pubkey: &str, json: bool) -> crate::cli::Result<()> {
    let resp = client::send(
        socket,
        &Request::Logout {
            pubkey: pubkey.to_string(),
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

pub async fn whoami(socket: &Path, json: bool) -> crate::cli::Result<()> {
    let resp = client::send(socket, &Request::AllAccounts).await?;
    if json {
        output::print_response(&resp, true);
    } else if let Some(accounts) = resp.result.as_ref().and_then(|v| v.as_array()) {
        if accounts.is_empty() {
            println!("No accounts logged in.");
        } else {
            for account in accounts {
                if let Some(pubkey) = account.get("pubkey").and_then(|v| v.as_str()) {
                    println!("{pubkey}");
                }
            }
        }
    } else {
        output::print_response(&resp, false);
    }
    if resp.error.is_some() {
        std::process::exit(1);
    }
    Ok(())
}

pub async fn export_nsec(socket: &Path, pubkey: &str, json: bool) -> crate::cli::Result<()> {
    let resp = client::send(
        socket,
        &Request::ExportNsec {
            pubkey: pubkey.to_string(),
        },
    )
    .await?;
    output::print_and_exit(&resp, json)
}

fn read_nsec() -> crate::cli::Result<String> {
    let nsec = if io::stdin().is_terminal() {
        eprint!("Enter nsec: ");
        io::stderr().flush()?;
        let secret = rpassword::read_password()?;
        eprintln!(); // newline after hidden input
        secret
    } else {
        let mut buf = String::new();
        io::stdin().lock().read_line(&mut buf)?;
        buf
    };
    let nsec = nsec.trim().to_string();
    if nsec.is_empty() {
        return Err(crate::cli::error::CliError::msg("no nsec provided"));
    }
    Ok(nsec)
}

fn print_login_success(resp: &Response) {
    if let Some(pubkey) = resp
        .result
        .as_ref()
        .and_then(|v| v.get("account"))
        .and_then(|v| v.get("pubkey"))
        .and_then(|v| v.as_str())
    {
        println!("Logged in as {pubkey}");
    }
}

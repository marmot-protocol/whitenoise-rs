use std::path::Path;

use crate::cli::client;
use crate::cli::error::CliError;
use crate::cli::protocol::Request;

/// Resolve which account to use for a command.
///
/// Resolution order (highest priority first):
/// 1. Explicit `--account <npub>` flag
/// 2. `WN_ACCOUNT` environment variable
/// 3. If exactly one account is logged in, use it
/// 4. Error with guidance
pub async fn resolve_account(socket: &Path, explicit: Option<&str>) -> crate::cli::Result<String> {
    // 1. Explicit flag
    if let Some(pubkey) = explicit {
        return Ok(pubkey.to_string());
    }

    // 2. Environment variable
    if let Some(pubkey) = std::env::var("WN_ACCOUNT")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        return Ok(pubkey);
    }

    // 3. Query daemon for account list and auto-resolve
    let resp = client::send(socket, &Request::AllAccounts).await?;

    if let Some(error) = &resp.error {
        return Err(CliError::msg(error.message.clone()));
    }

    let pubkeys = extract_pubkeys(&resp.result);
    resolve_from_list(&pubkeys)
}

/// Pure resolution logic over a list of pubkeys, separated for testability.
fn resolve_from_list(pubkeys: &[String]) -> crate::cli::Result<String> {
    match pubkeys.len() {
        0 => Err(CliError::msg(
            "no accounts logged in\n\n  Create one with: wn create-identity\n  Or log in with:  wn login",
        )),
        1 => Ok(pubkeys[0].clone()),
        _ => Err(CliError::msg(format!(
            "multiple accounts logged in. Specify --account <npub> or set WN_ACCOUNT\n\n  Accounts:\n{}",
            pubkeys
                .iter()
                .map(|p| format!("    {p}"))
                .collect::<Vec<_>>()
                .join("\n")
        ))),
    }
}

fn extract_pubkeys(result: &Option<serde_json::Value>) -> Vec<String> {
    result
        .as_ref()
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    item.get("pubkey")
                        .and_then(|p| p.as_str())
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_single_account() {
        let pubkeys = vec!["abc123".to_string()];
        let result = resolve_from_list(&pubkeys).unwrap();
        assert_eq!(result, "abc123");
    }

    #[test]
    fn resolve_no_accounts_errors() {
        let pubkeys: Vec<String> = vec![];
        let err = resolve_from_list(&pubkeys).unwrap_err();
        assert!(err.to_string().contains("no accounts logged in"));
    }

    #[test]
    fn resolve_multiple_accounts_errors() {
        let pubkeys = vec!["abc123".to_string(), "def456".to_string()];
        let err = resolve_from_list(&pubkeys).unwrap_err();
        assert!(err.to_string().contains("multiple accounts"));
        assert!(err.to_string().contains("abc123"));
        assert!(err.to_string().contains("def456"));
    }

    #[test]
    fn extract_pubkeys_from_response() {
        let val = serde_json::json!([
            {"pubkey": "abc123", "id": 1},
            {"pubkey": "def456", "id": 2},
        ]);
        let pubkeys = extract_pubkeys(&Some(val));
        assert_eq!(pubkeys, vec!["abc123", "def456"]);
    }

    #[test]
    fn extract_pubkeys_empty_array() {
        let val = serde_json::json!([]);
        let pubkeys = extract_pubkeys(&Some(val));
        assert!(pubkeys.is_empty());
    }

    #[test]
    fn extract_pubkeys_none() {
        let pubkeys = extract_pubkeys(&None);
        assert!(pubkeys.is_empty());
    }
}

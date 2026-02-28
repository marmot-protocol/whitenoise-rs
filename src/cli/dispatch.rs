use nostr_sdk::PublicKey;

use crate::Whitenoise;

use super::protocol::{Request, Response};

/// Route a request to the appropriate `Whitenoise` method and produce a response.
pub async fn dispatch(req: Request) -> Response {
    let wn = match Whitenoise::get_instance() {
        Ok(wn) => wn,
        Err(e) => return Response::err(format!("whitenoise not initialized: {e}")),
    };

    match req {
        Request::Ping => Response::ok(serde_json::json!("pong")),

        Request::CreateIdentity => match wn.create_identity().await {
            Ok(account) => to_response(&account),
            Err(e) => Response::err(e.to_string()),
        },

        Request::LoginStart { nsec } => match wn.login_start(nsec).await {
            Ok(result) => to_response(&result),
            Err(e) => Response::err(e.to_string()),
        },

        Request::LoginPublishDefaultRelays { pubkey } => match parse_pubkey(&pubkey) {
            Ok(pk) => match wn.login_publish_default_relays(&pk).await {
                Ok(result) => to_response(&result),
                Err(e) => Response::err(e.to_string()),
            },
            Err(resp) => resp,
        },

        Request::LoginWithCustomRelay { pubkey, relay_url } => match parse_pubkey(&pubkey) {
            Ok(pk) => match relay_url.parse::<nostr_sdk::RelayUrl>() {
                Ok(url) => match wn.login_with_custom_relay(&pk, url).await {
                    Ok(result) => to_response(&result),
                    Err(e) => Response::err(e.to_string()),
                },
                Err(e) => Response::err(format!("invalid relay URL: {e}")),
            },
            Err(resp) => resp,
        },

        Request::LoginCancel { pubkey } => match parse_pubkey(&pubkey) {
            Ok(pk) => match wn.login_cancel(&pk).await {
                Ok(()) => Response::ok(serde_json::json!(null)),
                Err(e) => Response::err(e.to_string()),
            },
            Err(resp) => resp,
        },

        Request::Logout { pubkey } => match parse_pubkey(&pubkey) {
            Ok(pk) => match wn.logout(&pk).await {
                Ok(()) => Response::ok(serde_json::json!(null)),
                Err(e) => Response::err(e.to_string()),
            },
            Err(resp) => resp,
        },

        Request::AllAccounts => match wn.all_accounts().await {
            Ok(accounts) => to_response(&accounts),
            Err(e) => Response::err(e.to_string()),
        },

        Request::ExportNsec { pubkey } => match parse_pubkey(&pubkey) {
            Ok(pk) => match wn.all_accounts().await {
                Ok(accounts) => match accounts.into_iter().find(|a| a.pubkey == pk) {
                    Some(account) => match wn.export_account_nsec(&account).await {
                        Ok(nsec) => to_response(&nsec),
                        Err(e) => Response::err(e.to_string()),
                    },
                    None => Response::err(format!("account not found: {pubkey}")),
                },
                Err(e) => Response::err(e.to_string()),
            },
            Err(resp) => resp,
        },
    }
}

fn parse_pubkey(s: &str) -> Result<PublicKey, Response> {
    PublicKey::parse(s).map_err(|e| Response::err(format!("invalid pubkey: {e}")))
}

fn to_response<T: serde::Serialize>(value: &T) -> Response {
    match serde_json::to_value(value) {
        Ok(v) => Response::ok(v),
        Err(e) => Response::err(format!("serialization error: {e}")),
    }
}

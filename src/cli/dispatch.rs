use std::collections::HashMap;

use mdk_core::prelude::{GroupId, NostrGroupConfigData};
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

        Request::ExportNsec { pubkey } => match find_account(wn, &pubkey).await {
            Ok(account) => match wn.export_account_nsec(&account).await {
                Ok(nsec) => to_response(&nsec),
                Err(e) => Response::err(e.to_string()),
            },
            Err(resp) => resp,
        },

        Request::VisibleGroups { account } => match find_account(wn, &account).await {
            Ok(acct) => match wn.visible_groups(&acct).await {
                Ok(groups) => to_response(&groups),
                Err(e) => Response::err(e.to_string()),
            },
            Err(resp) => resp,
        },

        Request::CreateGroup {
            account,
            name,
            description,
            members,
        } => match create_group(wn, &account, name, description, members).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::AddMembers {
            account,
            group_id,
            members,
        } => match add_members(wn, &account, &group_id, members).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::GetGroup { account, group_id } => match get_group(wn, &account, &group_id).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::ListMessages { account, group_id } => {
            match list_messages(wn, &account, &group_id).await {
                Ok(resp) => resp,
                Err(resp) => resp,
            }
        }

        Request::SendMessage {
            account,
            group_id,
            message,
        } => match send_message(wn, &account, &group_id, message).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },
    }
}

fn parse_pubkey(s: &str) -> Result<PublicKey, Response> {
    PublicKey::parse(s).map_err(|e| Response::err(format!("invalid pubkey: {e}")))
}

async fn find_account(
    wn: &Whitenoise,
    pubkey_str: &str,
) -> Result<crate::whitenoise::accounts::Account, Response> {
    let pk = parse_pubkey(pubkey_str)?;
    let accounts = wn
        .all_accounts()
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    accounts
        .into_iter()
        .find(|a| a.pubkey == pk)
        .ok_or_else(|| Response::err(format!("account not found: {pubkey_str}")))
}

async fn create_group(
    wn: &Whitenoise,
    account_str: &str,
    name: String,
    description: Option<String>,
    member_strs: Vec<String>,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;

    let member_pubkeys: Vec<PublicKey> = member_strs
        .iter()
        .map(|s| parse_pubkey(s))
        .collect::<Result<Vec<_>, _>>()?;

    let relays = account
        .inbox_relays(wn)
        .await
        .map_err(|e| Response::err(format!("failed to get relays: {e}")))?;
    let relay_urls = relays.into_iter().map(|r| r.url).collect();

    let config = NostrGroupConfigData::new(
        name,
        description.unwrap_or_default(),
        None, // image_hash
        None, // image_key
        None, // image_nonce
        relay_urls,
        vec![account.pubkey], // admins — creator only
    );

    let group = wn
        .create_group(&account, member_pubkeys, config, None)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(to_response(&group))
}

async fn add_members(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    member_strs: Vec<String>,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;
    let members: Vec<PublicKey> = member_strs
        .iter()
        .map(|s| parse_pubkey(s))
        .collect::<Result<Vec<_>, _>>()?;

    wn.add_members_to_group(&account, &group_id, members)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(Response::ok(serde_json::json!(null)))
}

async fn get_group(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;
    let group = wn
        .group(&account, &group_id)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(to_response(&group))
}

async fn list_messages(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;
    let messages = wn
        .fetch_aggregated_messages_for_group(&account.pubkey, &group_id)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    // Resolve author display names
    let unique_authors: Vec<PublicKey> = {
        let mut seen = std::collections::HashSet::new();
        messages
            .iter()
            .filter(|m| seen.insert(m.author))
            .map(|m| m.author)
            .collect()
    };
    let mut display_names: HashMap<PublicKey, String> = HashMap::new();
    for author in &unique_authors {
        if let Ok(user) = wn.find_user_by_pubkey(author).await {
            let name = user
                .metadata
                .display_name
                .as_ref()
                .filter(|s| !s.is_empty())
                .or(user.metadata.name.as_ref().filter(|s| !s.is_empty()));
            if let Some(name) = name {
                display_names.insert(*author, name.clone());
            }
        }
    }

    // Build clean response, filtering deleted messages
    let clean: Vec<serde_json::Value> = messages
        .iter()
        .filter(|m| !m.is_deleted)
        .map(|m| {
            let display_name = display_names
                .get(&m.author)
                .cloned()
                .unwrap_or_else(|| m.author.to_string());

            let created_at_local =
                chrono::DateTime::from_timestamp(m.created_at.as_secs() as i64, 0)
                    .map(|dt| {
                        dt.with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M:%S")
                            .to_string()
                    })
                    .unwrap_or_else(|| m.created_at.to_string());

            let mut msg = serde_json::json!({
                "id": m.id,
                "author": m.author.to_hex(),
                "display_name": display_name,
                "content": m.content,
                "created_at": m.created_at.as_secs(),
                "created_at_local": created_at_local,
                "kind": m.kind,
                "reply_to": m.reply_to_id,
                "is_reply": m.is_reply,
                "media_attachments": m.media_attachments,
            });

            // Only include reactions when non-empty
            if !m.reactions.by_emoji.is_empty() || !m.reactions.user_reactions.is_empty() {
                msg["reactions"] = serde_json::to_value(&m.reactions).unwrap_or_default();
            }

            msg
        })
        .collect();

    Ok(to_response(&clean))
}

async fn send_message(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    message: String,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;

    // Replicate send_message_to_group steps inline so we can publish
    // synchronously and report relay results back to the CLI user.
    let (inner_event, event_id) = wn
        .create_unsigned_nostr_event(&account.pubkey, &message, 9, None)
        .map_err(|e| Response::err(format!("create event: {e}")))?;

    let mdk = wn
        .create_mdk_for_account(account.pubkey)
        .map_err(|e| Response::err(format!("create mdk: {e}")))?;

    let message_event = mdk
        .create_message(&group_id, inner_event)
        .map_err(|e| Response::err(format!("create MLS message: {e}")))?;

    let stored_message = mdk
        .get_message(&group_id, &event_id)
        .map_err(|e| Response::err(format!("get message: {e}")))?
        .ok_or_else(|| Response::err("message not found after creation"))?;

    let group_relays: Vec<nostr_sdk::RelayUrl> = mdk
        .get_relays(&group_id)
        .map_err(|e| Response::err(format!("get relays: {e}")))?
        .into_iter()
        .collect();

    if group_relays.is_empty() {
        return Err(Response::err("no relays configured for this group"));
    }

    // Publish synchronously so the CLI gets actual relay feedback
    let publish_result = wn
        .nostr
        .publish_event_to(message_event, &account.pubkey, &group_relays)
        .await
        .map_err(|e| Response::err(format!("publish failed: {e}")))?;

    // Cache the message locally. The sender's own nostr-sdk client
    // deduplicates events it already published, so the subscription
    // will never deliver our own event back to us.
    wn.cache_chat_message(&group_id, &stored_message)
        .await
        .map_err(|e| Response::err(format!("cache message: {e}")))?;

    Ok(Response::ok(serde_json::json!({
        "id": stored_message.id,
        "content": stored_message.content,
        "relays_success": publish_result.success.len(),
        "relays_failed": publish_result.failed.len(),
    })))
}

fn parse_group_id(s: &str) -> Result<GroupId, Response> {
    let bytes = hex::decode(s).map_err(|e| Response::err(format!("invalid group ID: {e}")))?;
    Ok(GroupId::from_slice(&bytes))
}

fn to_response<T: serde::Serialize>(value: &T) -> Response {
    match serde_json::to_value(value) {
        Ok(v) => Response::ok(v),
        Err(e) => Response::err(format!("serialization error: {e}")),
    }
}

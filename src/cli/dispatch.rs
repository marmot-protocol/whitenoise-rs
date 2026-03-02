use std::collections::HashMap;

use mdk_core::prelude::{GroupId, NostrGroupConfigData};
use nostr_sdk::{JsonUtil, PublicKey};

use crate::Whitenoise;
use crate::whitenoise::accounts_groups::AccountGroup;

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

        Request::GroupMembers { account, group_id } => {
            match group_pubkey_list(wn, &account, &group_id, false).await {
                Ok(resp) => resp,
                Err(resp) => resp,
            }
        }

        Request::GroupAdmins { account, group_id } => {
            match group_pubkey_list(wn, &account, &group_id, true).await {
                Ok(resp) => resp,
                Err(resp) => resp,
            }
        }

        Request::RemoveMembers {
            account,
            group_id,
            members,
        } => match remove_members(wn, &account, &group_id, members).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::LeaveGroup { account, group_id } => {
            match leave_group(wn, &account, &group_id).await {
                Ok(resp) => resp,
                Err(resp) => resp,
            }
        }

        Request::RenameGroup {
            account,
            group_id,
            name,
        } => match rename_group(wn, &account, &group_id, name).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::GroupInvites { account } => match group_invites(wn, &account).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::AcceptInvite { account, group_id } => {
            match respond_to_invite(wn, &account, &group_id, true).await {
                Ok(resp) => resp,
                Err(resp) => resp,
            }
        }

        Request::DeclineInvite { account, group_id } => {
            match respond_to_invite(wn, &account, &group_id, false).await {
                Ok(resp) => resp,
                Err(resp) => resp,
            }
        }

        Request::ProfileShow { account } => match profile_show(wn, &account).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::ProfileUpdate {
            account,
            name,
            display_name,
            about,
            picture,
            nip05,
            lud16,
        } => {
            match profile_update(
                wn,
                &account,
                name,
                display_name,
                about,
                picture,
                nip05,
                lud16,
            )
            .await
            {
                Ok(resp) => resp,
                Err(resp) => resp,
            }
        }

        Request::FollowsList { account } => match follows_list(wn, &account).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::FollowsAdd { account, pubkey } => {
            match follows_mutate(wn, &account, &pubkey, FollowAction::Add).await {
                Ok(resp) => resp,
                Err(resp) => resp,
            }
        }

        Request::FollowsRemove { account, pubkey } => {
            match follows_mutate(wn, &account, &pubkey, FollowAction::Remove).await {
                Ok(resp) => resp,
                Err(resp) => resp,
            }
        }

        Request::FollowsCheck { account, pubkey } => {
            match follows_check(wn, &account, &pubkey).await {
                Ok(resp) => resp,
                Err(resp) => resp,
            }
        }

        Request::ChatsList { account } => match chats_list(wn, &account).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::SettingsShow => match wn.app_settings().await {
            Ok(settings) => to_response(&settings),
            Err(e) => Response::err(e.to_string()),
        },

        Request::SettingsTheme { theme } => match settings_theme(wn, &theme).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::SettingsLanguage { language } => match settings_language(wn, &language).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::RelaysList { account } => match relays_list(wn, &account).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::UsersShow { pubkey } => match users_show(wn, &pubkey).await {
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

async fn resolve_display_name(wn: &Whitenoise, pubkey: &PublicKey) -> Option<String> {
    // Try local User record first
    if let Ok(mut user) = wn.find_user_by_pubkey(pubkey).await {
        if user.metadata.display_name.is_none() && user.metadata.name.is_none() {
            let _ = user.sync_metadata(wn).await;
        }
        let name = user
            .metadata
            .display_name
            .as_ref()
            .filter(|s| !s.is_empty())
            .or(user.metadata.name.as_ref().filter(|s| !s.is_empty()))
            .cloned();
        if name.is_some() {
            return name;
        }
    }

    // User not in DB or sync returned empty — fetch metadata directly from relays
    let relays = wn.fallback_relay_urls().await;
    let event = wn
        .nostr
        .fetch_metadata_from(&relays, *pubkey)
        .await
        .ok()??;
    let metadata = nostr_sdk::Metadata::from_json(&event.content).ok()?;
    metadata
        .display_name
        .as_ref()
        .filter(|s| !s.is_empty())
        .or(metadata.name.as_ref().filter(|s| !s.is_empty()))
        .cloned()
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

async fn group_pubkey_list(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    admins_only: bool,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;
    let pubkeys = if admins_only {
        wn.group_admins(&account, &group_id).await
    } else {
        wn.group_members(&account, &group_id).await
    }
    .map_err(|e| Response::err(e.to_string()))?;

    let mut members = Vec::with_capacity(pubkeys.len());
    for pk in &pubkeys {
        let display_name = resolve_display_name(wn, pk).await;
        members.push(serde_json::json!({
            "pubkey": pk.to_hex(),
            "display_name": display_name,
        }));
    }
    Ok(to_response(&members))
}

async fn remove_members(
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

    wn.remove_members_from_group(&account, &group_id, members)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(Response::ok(serde_json::json!(null)))
}

async fn leave_group(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;
    wn.leave_group(&account, &group_id)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(Response::ok(serde_json::json!(null)))
}

async fn rename_group(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    name: String,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;
    let update = mdk_core::prelude::NostrGroupDataUpdate::new().name(name);
    wn.update_group_data(&account, &group_id, update)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(Response::ok(serde_json::json!(null)))
}

async fn group_invites(wn: &Whitenoise, account_str: &str) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let groups = wn
        .visible_groups(&account)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    let pending: Vec<_> = groups.into_iter().filter(|g| g.is_pending()).collect();

    Ok(to_response(&pending))
}

async fn respond_to_invite(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    accept: bool,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;

    let ag = AccountGroup::get(wn, &account.pubkey, &group_id)
        .await
        .map_err(|e| Response::err(e.to_string()))?
        .ok_or_else(|| Response::err("group not found for this account"))?;

    if accept {
        ag.accept(wn).await
    } else {
        ag.decline(wn).await
    }
    .map_err(|e| Response::err(e.to_string()))?;

    Ok(Response::ok(serde_json::json!(null)))
}

async fn profile_show(wn: &Whitenoise, account_str: &str) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let metadata = account
        .metadata(wn)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(to_response(&metadata))
}

#[allow(clippy::too_many_arguments)]
async fn profile_update(
    wn: &Whitenoise,
    account_str: &str,
    name: Option<String>,
    display_name: Option<String>,
    about: Option<String>,
    picture: Option<String>,
    nip05: Option<String>,
    lud16: Option<String>,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;

    // Read-modify-write: start from current metadata, apply provided fields
    let mut metadata = account
        .metadata(wn)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    if let Some(v) = name {
        metadata.name = Some(v);
    }
    if let Some(v) = display_name {
        metadata.display_name = Some(v);
    }
    if let Some(v) = about {
        metadata.about = Some(v);
    }
    if let Some(v) = picture {
        metadata.picture = Some(
            v.parse()
                .map_err(|e| Response::err(format!("invalid picture URL: {e}")))?,
        );
    }
    if let Some(v) = nip05 {
        metadata.nip05 = Some(v);
    }
    if let Some(v) = lud16 {
        metadata.lud16 = Some(v);
    }

    account
        .update_metadata(&metadata, wn)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(to_response(&metadata))
}

async fn follows_list(wn: &Whitenoise, account_str: &str) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let users = wn
        .follows(&account)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    let mut entries = Vec::with_capacity(users.len());
    for user in &users {
        let display_name = resolve_display_name(wn, &user.pubkey).await;
        entries.push(serde_json::json!({
            "pubkey": user.pubkey.to_hex(),
            "display_name": display_name,
        }));
    }
    Ok(to_response(&entries))
}

enum FollowAction {
    Add,
    Remove,
}

async fn follows_mutate(
    wn: &Whitenoise,
    account_str: &str,
    pubkey_str: &str,
    action: FollowAction,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let pubkey = parse_pubkey(pubkey_str)?;
    match action {
        FollowAction::Add => wn.follow_user(&account, &pubkey).await,
        FollowAction::Remove => wn.unfollow_user(&account, &pubkey).await,
    }
    .map_err(|e| Response::err(e.to_string()))?;
    Ok(Response::ok(serde_json::json!(null)))
}

async fn follows_check(
    wn: &Whitenoise,
    account_str: &str,
    pubkey_str: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let pubkey = parse_pubkey(pubkey_str)?;
    let following = wn
        .is_following_user(&account, &pubkey)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(Response::ok(serde_json::json!({ "following": following })))
}

async fn chats_list(wn: &Whitenoise, account_str: &str) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let items = wn
        .get_chat_list(&account)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    // Strip redundant mls_group_id from last_message (already on the outer item)
    let mut value = serde_json::to_value(&items).map_err(|e| Response::err(e.to_string()))?;
    if let Some(arr) = value.as_array_mut() {
        for item in arr {
            if let Some(last_msg) = item.get_mut("last_message") {
                if let Some(obj) = last_msg.as_object_mut() {
                    obj.remove("mls_group_id");
                }
            }
        }
    }

    Ok(Response::ok(value))
}

async fn settings_theme(wn: &Whitenoise, theme_str: &str) -> Result<Response, Response> {
    use crate::whitenoise::app_settings::ThemeMode;
    let theme: ThemeMode = theme_str
        .parse()
        .map_err(|e: String| Response::err(e))?;
    wn.update_theme_mode(theme)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(Response::ok(serde_json::json!(null)))
}

async fn settings_language(wn: &Whitenoise, lang_str: &str) -> Result<Response, Response> {
    use crate::whitenoise::app_settings::Language;
    let language: Language = lang_str
        .parse()
        .map_err(|e: String| Response::err(e))?;
    wn.update_language(language)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(Response::ok(serde_json::json!(null)))
}

async fn relays_list(wn: &Whitenoise, account_str: &str) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let statuses = wn
        .get_account_relay_statuses(&account)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    let relays: Vec<serde_json::Value> = statuses
        .into_iter()
        .map(|(url, status)| {
            serde_json::json!({
                "url": url.to_string(),
                "status": format!("{status}"),
            })
        })
        .collect();

    Ok(to_response(&relays))
}

async fn users_show(wn: &Whitenoise, pubkey_str: &str) -> Result<Response, Response> {
    use crate::whitenoise::users::UserSyncMode;
    let pk = parse_pubkey(pubkey_str)?;
    let user = wn
        .find_or_create_user_by_pubkey(&pk, UserSyncMode::Blocking)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(to_response(&user))
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

    // Resolve display names for all referenced pubkeys (authors + reaction users)
    let unique_pubkeys: Vec<PublicKey> = {
        let mut seen = std::collections::HashSet::new();
        for m in &messages {
            seen.insert(m.author);
            for reaction in m.reactions.by_emoji.values() {
                for pk in &reaction.users {
                    seen.insert(*pk);
                }
            }
        }
        seen.into_iter().collect()
    };
    let mut display_names: HashMap<PublicKey, String> = HashMap::new();
    for pk in &unique_pubkeys {
        if let Some(name) = resolve_display_name(wn, pk).await {
            display_names.insert(*pk, name);
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
                // Build by_emoji with display names instead of raw pubkeys
                let by_emoji: serde_json::Map<String, serde_json::Value> = m
                    .reactions
                    .by_emoji
                    .iter()
                    .map(|(emoji, reaction)| {
                        let user_names: Vec<String> = reaction
                            .users
                            .iter()
                            .map(|pk| {
                                display_names
                                    .get(pk)
                                    .cloned()
                                    .unwrap_or_else(|| pk.to_hex())
                            })
                            .collect();
                        (
                            emoji.clone(),
                            serde_json::json!({
                                "emoji": reaction.emoji,
                                "count": reaction.count,
                                "users": user_names,
                            }),
                        )
                    })
                    .collect();

                let mut reactions = serde_json::to_value(&m.reactions).unwrap_or_default();
                reactions["by_emoji"] = serde_json::Value::Object(by_emoji);
                msg["reactions"] = reactions;
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

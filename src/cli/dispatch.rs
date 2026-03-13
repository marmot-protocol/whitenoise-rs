use std::collections::HashMap;

use mdk_core::prelude::{GroupId, NostrGroupConfigData, NostrGroupDataUpdate};
use nostr_sdk::{PublicKey, RelayUrl, Timestamp};
use tokio::io::AsyncWriteExt;

use crate::Whitenoise;
use crate::whitenoise::accounts_groups::AccountGroup;
use crate::whitenoise::app_settings::{Language, ThemeMode};
use crate::whitenoise::relays::{Relay, RelayType};
use crate::whitenoise::users::{KeyPackageStatus, UserSyncMode};

use super::protocol::{Request, Response};

/// Route a request to the appropriate `Whitenoise` method and produce a response.
pub async fn dispatch(req: Request) -> Response {
    let wn = match Whitenoise::get_instance() {
        Ok(wn) => wn,
        Err(e) => return Response::err(format!("whitenoise not initialized: {e}")),
    };

    match req {
        Request::Ping => Response::ok(serde_json::json!("pong")),

        Request::DebugRelayControlState => {
            match serde_json::to_value(wn.get_relay_control_state().await) {
                Ok(snapshot) => Response::ok(snapshot),
                Err(e) => Response::err(e.to_string()),
            }
        }

        Request::DebugHealth { account } => match debug_health(wn, &account).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::DebugRatchetTree { account, group_id } => {
            match debug_ratchet_tree(wn, &account, &group_id).await {
                Ok(resp) => resp,
                Err(resp) => resp,
            }
        }

        Request::DeleteAllData => match wn.delete_all_data().await {
            Ok(()) => Response::ok(serde_json::json!({
                "message": "all data deleted; restart the daemon"
            })),
            Err(e) => Response::err(e.to_string()),
        },

        Request::GroupPromote {
            account,
            group_id,
            pubkey,
        } => match promote_admin(wn, &account, &group_id, &pubkey).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::GroupDemote {
            account,
            group_id,
            pubkey,
        } => match demote_admin(wn, &account, &group_id, &pubkey).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::KeysList { account } => match keys_list(wn, &account).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::KeysPublish { account } => match keys_publish(wn, &account).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::KeysDelete { account, event_id } => {
            match keys_delete(wn, &account, &event_id).await {
                Ok(resp) => resp,
                Err(resp) => resp,
            }
        }

        Request::KeysDeleteAll { account } => match keys_delete_all(wn, &account).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::KeysCheck { pubkey } => match keys_check(wn, &pubkey).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

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

        Request::GroupRelays { account, group_id } => {
            match group_relay_list(wn, &account, &group_id).await {
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

        Request::ArchiveChat { account, group_id } => {
            match archive_chat(wn, &account, &group_id).await {
                Ok(resp) => resp,
                Err(resp) => resp,
            }
        }

        Request::UnarchiveChat { account, group_id } => {
            match unarchive_chat(wn, &account, &group_id).await {
                Ok(resp) => resp,
                Err(resp) => resp,
            }
        }

        Request::ArchivedChatsList { account } => match archived_chats_list(wn, &account).await {
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

        Request::RelaysList {
            account,
            relay_type,
        } => match relays_list(wn, &account, relay_type.as_deref()).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::RelaysAdd {
            account,
            url,
            relay_type,
        } => match relays_add(wn, &account, &url, &relay_type).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::RelaysRemove {
            account,
            url,
            relay_type,
        } => match relays_remove(wn, &account, &url, &relay_type).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::UsersShow { pubkey } => match users_show(wn, &pubkey).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::ListMessages {
            account,
            group_id,
            before,
            before_message_id,
            limit,
        } => match list_messages(
            wn,
            &account,
            &group_id,
            before,
            before_message_id.as_deref(),
            limit,
        )
        .await
        {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::SearchMessages {
            account,
            group_id,
            query,
            limit,
        } => match search_messages(wn, &account, &group_id, &query, limit).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::SendMessage {
            account,
            group_id,
            message,
            reply_to,
        } => match send_message(wn, &account, &group_id, message, reply_to).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::DeleteMessage {
            account,
            group_id,
            message_id,
        } => match delete_message(wn, &account, &group_id, &message_id).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::RetryMessage {
            account,
            group_id,
            event_id,
        } => match retry_message(wn, &account, &group_id, &event_id).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::ReactToMessage {
            account,
            group_id,
            message_id,
            emoji,
        } => match react_to_message(wn, &account, &group_id, &message_id, emoji).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::UnreactToMessage {
            account,
            group_id,
            message_id,
        } => match unreact_to_message(wn, &account, &group_id, &message_id).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::UploadMedia {
            account,
            group_id,
            file_path,
            send,
            message,
        } => match upload_media(wn, &account, &group_id, &file_path, send, message).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::DownloadMedia {
            account,
            group_id,
            file_hash,
        } => match download_media(wn, &account, &group_id, &file_hash).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        Request::ListMedia { group_id } => match list_media(wn, &group_id).await {
            Ok(resp) => resp,
            Err(resp) => resp,
        },

        // Streaming commands should be routed through dispatch_streaming
        Request::MessagesSubscribe { .. }
        | Request::ChatsSubscribe { .. }
        | Request::ArchivedChatsSubscribe { .. }
        | Request::NotificationsSubscribe
        | Request::UsersSearch { .. } => {
            Response::err("streaming commands must use dispatch_streaming")
        }
    }
}

/// Write a single response line to the writer. Returns false if the client disconnected.
async fn write_response<W>(writer: &mut W, response: &Response) -> bool
where
    W: AsyncWriteExt + Unpin,
{
    let mut buf = match serde_json::to_vec(response) {
        Ok(buf) => buf,
        Err(e) => {
            tracing::error!("failed to serialize response: {e}");
            return false;
        }
    };
    buf.push(b'\n');
    writer.write_all(&buf).await.is_ok()
}

/// Write the `stream_end: true` sentinel that signals the end of a streaming response.
async fn write_stream_end<W>(writer: &mut W)
where
    W: AsyncWriteExt + Unpin,
{
    let end = Response {
        result: None,
        error: None,
        stream_end: true,
    };
    let _ = write_response(writer, &end).await;
}

/// Handle a streaming request by writing multiple response lines to the writer.
///
/// This function takes ownership of the writer and writes response lines until
/// the stream ends or the client disconnects. The final line has `stream_end: true`.
pub async fn dispatch_streaming<W>(req: Request, mut writer: W)
where
    W: AsyncWriteExt + Unpin + Send,
{
    let wn = match Whitenoise::get_instance() {
        Ok(wn) => wn,
        Err(e) => {
            let _ = write_response(
                &mut writer,
                &Response::err(format!("whitenoise not initialized: {e}")),
            )
            .await;
            write_stream_end(&mut writer).await;
            return;
        }
    };

    match req {
        Request::MessagesSubscribe { account, group_id } => {
            messages_subscribe(wn, &mut writer, &account, &group_id).await;
        }
        Request::ChatsSubscribe { account } => {
            chats_subscribe(wn, &mut writer, &account).await;
        }
        Request::ArchivedChatsSubscribe { account } => {
            archived_chats_subscribe(wn, &mut writer, &account).await;
        }
        Request::NotificationsSubscribe => {
            notifications_subscribe(wn, &mut writer).await;
        }
        Request::UsersSearch {
            account,
            query,
            radius_start,
            radius_end,
        } => {
            users_search(wn, &mut writer, &account, &query, radius_start, radius_end).await;
        }
        _ => {
            let _ = write_response(&mut writer, &Response::err("not a streaming command")).await;
            write_stream_end(&mut writer).await;
        }
    }
}

async fn messages_subscribe<W>(
    wn: &Whitenoise,
    writer: &mut W,
    account_str: &str,
    group_id_hex: &str,
) where
    W: AsyncWriteExt + Unpin,
{
    // Validate account and group_id
    let _account = match find_account(wn, account_str).await {
        Ok(a) => a,
        Err(resp) => {
            let _ = write_response(writer, &resp).await;
            write_stream_end(writer).await;
            return;
        }
    };
    let group_id = match parse_group_id(group_id_hex) {
        Ok(id) => id,
        Err(resp) => {
            let _ = write_response(writer, &resp).await;
            write_stream_end(writer).await;
            return;
        }
    };

    // Subscribe — gets initial snapshot + broadcast receiver
    let subscription = match wn.subscribe_to_group_messages(&group_id).await {
        Ok(sub) => sub,
        Err(e) => {
            let _ = write_response(writer, &Response::err(e.to_string())).await;
            write_stream_end(writer).await;
            return;
        }
    };

    // Resolve display names for initial messages
    let mut display_names = resolve_chat_display_names(wn, &subscription.initial_messages).await;

    // Send initial messages in chronological order (oldest first, newest last)
    for msg in &subscription.initial_messages {
        if let Some(formatted) = format_chat_message(msg, &display_names) {
            let resp = Response::ok(serde_json::json!({
                "trigger": "InitialMessage",
                "message": formatted,
            }));
            if !write_response(writer, &resp).await {
                return; // Client disconnected
            }
        }
    }

    // Stream live updates
    let mut updates = subscription.updates;
    loop {
        match updates.recv().await {
            Ok(update) => {
                // Resolve display name for new authors on the fly
                if let std::collections::hash_map::Entry::Vacant(e) =
                    display_names.entry(update.message.author)
                    && let Some(name) = resolve_display_name(wn, &update.message.author).await
                {
                    e.insert(name);
                }

                if let Some(formatted) = format_chat_message(&update.message, &display_names) {
                    let resp = Response::ok(serde_json::json!({
                        "trigger": update.trigger,
                        "message": formatted,
                    }));
                    if !write_response(writer, &resp).await {
                        return; // Client disconnected
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("message stream lagged by {n} messages");
                let _ = write_response(
                    writer,
                    &Response::err(format!("stream lagged: {n} messages dropped")),
                )
                .await;
            }
        }
    }

    write_stream_end(writer).await;
}

async fn chats_subscribe<W>(wn: &Whitenoise, writer: &mut W, account_str: &str)
where
    W: AsyncWriteExt + Unpin,
{
    let account = match find_account(wn, account_str).await {
        Ok(a) => a,
        Err(resp) => {
            let _ = write_response(writer, &resp).await;
            write_stream_end(writer).await;
            return;
        }
    };

    let subscription = match wn.subscribe_to_chat_list(&account).await {
        Ok(sub) => sub,
        Err(e) => {
            let _ = write_response(writer, &Response::err(e.to_string())).await;
            write_stream_end(writer).await;
            return;
        }
    };

    // Send initial items
    for item in &subscription.initial_items {
        let formatted = clean_chat_list_item(wn, item).await;
        let resp = Response::ok(serde_json::json!({
            "trigger": "InitialItem",
            "item": formatted,
        }));
        if !write_response(writer, &resp).await {
            return;
        }
    }

    // Stream live updates
    let mut updates = subscription.updates;
    loop {
        match updates.recv().await {
            Ok(update) => {
                let formatted = clean_chat_list_item(wn, &update.item).await;
                let resp = Response::ok(serde_json::json!({
                    "trigger": update.trigger,
                    "item": formatted,
                }));
                if !write_response(writer, &resp).await {
                    return;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("chat list stream lagged by {n} updates");
                let _ = write_response(
                    writer,
                    &Response::err(format!("stream lagged: {n} updates dropped")),
                )
                .await;
            }
        }
    }

    write_stream_end(writer).await;
}

async fn notifications_subscribe<W>(wn: &Whitenoise, writer: &mut W)
where
    W: AsyncWriteExt + Unpin,
{
    let subscription = wn.subscribe_to_notifications();

    let mut updates = subscription.updates;
    loop {
        match updates.recv().await {
            Ok(update) => {
                let resp = Response::ok(serde_json::json!({
                    "trigger": update.trigger,
                    "mls_group_id": update.mls_group_id,
                    "group_name": update.group_name,
                    "is_dm": update.is_dm,
                    "receiver": update.receiver,
                    "sender": update.sender,
                    "content": update.content,
                    "timestamp": update.timestamp,
                }));
                if !write_response(writer, &resp).await {
                    return;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("notification stream lagged by {n} updates");
                let _ = write_response(
                    writer,
                    &Response::err(format!("stream lagged: {n} notifications dropped")),
                )
                .await;
            }
        }
    }

    write_stream_end(writer).await;
}

async fn users_search<W>(
    wn: &Whitenoise,
    writer: &mut W,
    account_str: &str,
    query: &str,
    radius_start: u8,
    radius_end: u8,
) where
    W: AsyncWriteExt + Unpin,
{
    let account = match find_account(wn, account_str).await {
        Ok(a) => a,
        Err(resp) => {
            let _ = write_response(writer, &resp).await;
            write_stream_end(writer).await;
            return;
        }
    };

    let params = crate::whitenoise::user_search::UserSearchParams {
        query: query.to_string(),
        searcher_pubkey: account.pubkey,
        radius_start,
        radius_end,
    };

    let subscription = match wn.search_users(params).await {
        Ok(sub) => sub,
        Err(e) => {
            let _ = write_response(writer, &Response::err(e.to_string())).await;
            write_stream_end(writer).await;
            return;
        }
    };

    let mut updates = subscription.updates;
    loop {
        match updates.recv().await {
            Ok(update) => {
                let resp = to_response(&update);
                if !write_response(writer, &resp).await {
                    return; // Client disconnected
                }
                // Stop after SearchCompleted
                if matches!(
                    update.trigger,
                    crate::whitenoise::user_search::SearchUpdateTrigger::SearchCompleted { .. }
                ) {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("user search stream lagged by {n} updates");
            }
        }
    }

    write_stream_end(writer).await;
}

/// Clean a ChatListItem for output: strip redundant mls_group_id from last_message,
/// and resolve missing author_display_name.
async fn clean_chat_list_item(
    wn: &Whitenoise,
    item: &crate::whitenoise::chat_list::ChatListItem,
) -> serde_json::Value {
    let mut value = serde_json::to_value(item).unwrap_or_default();
    if let Some(last_msg) = value.get_mut("last_message")
        && let Some(obj) = last_msg.as_object_mut()
    {
        obj.remove("mls_group_id");

        // Resolve author_display_name if missing
        if obj.get("author_display_name").is_none_or(|v| v.is_null())
            && let Some(author_hex) = obj.get("author").and_then(|v| v.as_str())
            && let Ok(pk) = PublicKey::parse(author_hex)
            && let Some(name) = resolve_display_name(wn, &pk).await
        {
            obj.insert(
                "author_display_name".to_string(),
                serde_json::Value::String(name),
            );
        }
    }
    value
}

fn parse_pubkey(s: &str) -> Result<PublicKey, Response> {
    PublicKey::parse(s).map_err(|e| Response::err(format!("invalid pubkey '{s}': {e}")))
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
    let user = wn
        .find_or_create_user_by_pubkey(pubkey, UserSyncMode::Blocking)
        .await
        .ok()?;
    user.metadata
        .display_name
        .as_ref()
        .filter(|s| !s.is_empty())
        .or(user.metadata.name.as_ref().filter(|s| !s.is_empty()))
        .cloned()
}

fn cli_group_relay_urls() -> Vec<RelayUrl> {
    Relay::defaults()
        .into_iter()
        .map(|relay| relay.url)
        .collect()
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

    let config = NostrGroupConfigData::new(
        name,
        description.unwrap_or_default(),
        None, // image_hash
        None, // image_key
        None, // image_nonce
        cli_group_relay_urls(),
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

async fn group_relay_list(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;
    let relays = wn
        .group_relays(&account, &group_id)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    let urls: Vec<&str> = relays.iter().map(|r| r.as_str()).collect();
    Ok(to_response(&urls))
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
    let update = NostrGroupDataUpdate::new().name(name);
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

    let mut clean = Vec::with_capacity(items.len());
    for item in &items {
        clean.push(clean_chat_list_item(wn, item).await);
    }
    Ok(to_response(&clean))
}

async fn archive_chat(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;
    wn.archive_chat(&account, &group_id)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(Response::ok(serde_json::json!(null)))
}

async fn unarchive_chat(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;
    wn.unarchive_chat(&account, &group_id)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(Response::ok(serde_json::json!(null)))
}

async fn archived_chats_list(wn: &Whitenoise, account_str: &str) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let items = wn
        .get_archived_chat_list(&account)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    let mut clean = Vec::with_capacity(items.len());
    for item in &items {
        clean.push(clean_chat_list_item(wn, item).await);
    }
    Ok(to_response(&clean))
}

async fn archived_chats_subscribe<W>(wn: &Whitenoise, writer: &mut W, account_str: &str)
where
    W: AsyncWriteExt + Unpin,
{
    let account = match find_account(wn, account_str).await {
        Ok(a) => a,
        Err(resp) => {
            let _ = write_response(writer, &resp).await;
            write_stream_end(writer).await;
            return;
        }
    };

    let subscription = match wn.subscribe_to_archived_chat_list(&account).await {
        Ok(sub) => sub,
        Err(e) => {
            let _ = write_response(writer, &Response::err(e.to_string())).await;
            write_stream_end(writer).await;
            return;
        }
    };

    // Send initial items
    for item in &subscription.initial_items {
        let formatted = clean_chat_list_item(wn, item).await;
        let resp = Response::ok(serde_json::json!({
            "trigger": "InitialItem",
            "item": formatted,
        }));
        if !write_response(writer, &resp).await {
            return;
        }
    }

    // Stream live updates
    let mut updates = subscription.updates;
    loop {
        match updates.recv().await {
            Ok(update) => {
                let formatted = clean_chat_list_item(wn, &update.item).await;
                let resp = Response::ok(serde_json::json!({
                    "trigger": update.trigger,
                    "item": formatted,
                }));
                if !write_response(writer, &resp).await {
                    return;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("archived chat list stream lagged by {n} updates");
                let _ = write_response(
                    writer,
                    &Response::err(format!("stream lagged: {n} updates dropped")),
                )
                .await;
            }
        }
    }

    write_stream_end(writer).await;
}

async fn settings_theme(wn: &Whitenoise, theme_str: &str) -> Result<Response, Response> {
    let theme: ThemeMode = theme_str.parse().map_err(|e: String| Response::err(e))?;
    wn.update_theme_mode(theme)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(Response::ok(serde_json::json!(null)))
}

async fn settings_language(wn: &Whitenoise, lang_str: &str) -> Result<Response, Response> {
    let language: Language = lang_str.parse().map_err(|e: String| Response::err(e))?;
    wn.update_language(language)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(Response::ok(serde_json::json!(null)))
}

fn parse_relay_type(s: &str) -> Result<RelayType, Response> {
    s.parse::<RelayType>()
        .map_err(|e| Response::err(format!("{e}. Valid types: nip65, inbox, key_package")))
}

fn parse_relay_url(s: &str) -> Result<RelayUrl, Response> {
    RelayUrl::parse(s).map_err(|e| Response::err(format!("invalid relay URL: {e}")))
}

async fn relays_list(
    wn: &Whitenoise,
    account_str: &str,
    type_filter: Option<&str>,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;

    let types_to_query = match type_filter {
        Some(s) => vec![parse_relay_type(s)?],
        None => vec![RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage],
    };

    // Collect relay URLs and their associated types
    let mut relay_types: HashMap<RelayUrl, Vec<String>> = HashMap::new();
    for rt in &types_to_query {
        let relays = account
            .relays(*rt, wn)
            .await
            .map_err(|e| Response::err(e.to_string()))?;
        let type_str: String = (*rt).into();
        for relay in relays {
            relay_types
                .entry(relay.url)
                .or_default()
                .push(type_str.clone());
        }
    }

    // Get connection statuses for these relays
    let statuses = wn
        .get_account_relay_statuses(&account)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    let status_map: HashMap<_, _> = statuses.into_iter().collect();

    let mut relays: Vec<serde_json::Value> = relay_types
        .into_iter()
        .map(|(url, types)| {
            let status = status_map
                .get(&url)
                .map(|s| format!("{s}"))
                .unwrap_or_else(|| "Disconnected".to_string());
            serde_json::json!({
                "url": url.to_string(),
                "types": types,
                "status": status,
            })
        })
        .collect();
    relays.sort_by(|a, b| a["url"].as_str().cmp(&b["url"].as_str()));

    Ok(to_response(&relays))
}

async fn relays_add(
    wn: &Whitenoise,
    account_str: &str,
    url_str: &str,
    type_str: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let relay_type = parse_relay_type(type_str)?;
    let relay_url = parse_relay_url(url_str)?;
    let relay = wn
        .find_or_create_relay_by_url(&relay_url)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    account
        .add_relay(&relay, relay_type, wn)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(Response::ok(serde_json::json!({
        "url": relay_url.to_string(),
        "type": type_str,
    })))
}

async fn relays_remove(
    wn: &Whitenoise,
    account_str: &str,
    url_str: &str,
    type_str: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let relay_type = parse_relay_type(type_str)?;
    let relay_url = parse_relay_url(url_str)?;
    let relay = account
        .relays(relay_type, wn)
        .await
        .map_err(|e| Response::err(e.to_string()))?
        .into_iter()
        .find(|r| r.url == relay_url)
        .ok_or_else(|| Response::err(format!("relay not found for type {type_str}: {url_str}")))?;

    account
        .remove_relay(&relay, relay_type, wn)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(Response::ok(serde_json::json!({
        "url": relay_url.to_string(),
        "type": type_str,
    })))
}

async fn users_show(wn: &Whitenoise, pubkey_str: &str) -> Result<Response, Response> {
    let pk = parse_pubkey(pubkey_str)?;
    let user = wn
        .find_or_create_user_by_pubkey(&pk, UserSyncMode::Blocking)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(to_response(&user))
}

/// Collect unique pubkeys from a slice of ChatMessages (authors + reaction users)
/// and resolve their display names.
async fn resolve_chat_display_names(
    wn: &Whitenoise,
    messages: &[crate::whitenoise::message_aggregator::ChatMessage],
) -> HashMap<PublicKey, String> {
    let unique_pubkeys: Vec<PublicKey> = {
        let mut seen = std::collections::HashSet::new();
        for m in messages {
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
    display_names
}

/// Format a single ChatMessage into a JSON value with display names and local timestamps.
/// Skipped if the message is deleted (returns None).
fn format_chat_message(
    m: &crate::whitenoise::message_aggregator::ChatMessage,
    display_names: &HashMap<PublicKey, String>,
) -> Option<serde_json::Value> {
    if m.is_deleted {
        return None;
    }

    let display_name = display_names
        .get(&m.author)
        .cloned()
        .unwrap_or_else(|| m.author.to_string());

    let created_at_local = chrono::DateTime::from_timestamp(m.created_at.as_secs() as i64, 0)
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

    Some(msg)
}

async fn list_messages(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    before: Option<u64>,
    before_message_id: Option<&str>,
    limit: Option<u32>,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;
    let before_ts = before.map(Timestamp::from);
    let messages = wn
        .fetch_aggregated_messages_for_group(
            &account.pubkey,
            &group_id,
            before_ts,
            before_message_id,
            limit,
        )
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    let display_names = resolve_chat_display_names(wn, &messages).await;

    let clean: Vec<serde_json::Value> = messages
        .iter()
        .filter_map(|m| format_chat_message(m, &display_names))
        .collect();

    Ok(to_response(&clean))
}

async fn search_messages(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    query: &str,
    limit: Option<u32>,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;
    let results = wn
        .search_messages_in_group(&account.pubkey, &group_id, query, limit)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    let messages: Vec<crate::whitenoise::message_aggregator::ChatMessage> =
        results.iter().map(|r| r.message.clone()).collect();
    let display_names = resolve_chat_display_names(wn, &messages).await;

    let clean: Vec<serde_json::Value> = results
        .iter()
        .filter_map(|r| {
            let mut msg = format_chat_message(&r.message, &display_names)?;
            msg["highlight_spans"] = serde_json::to_value(&r.highlight_spans)
                .unwrap_or(serde_json::Value::Array(vec![]));
            Some(msg)
        })
        .collect();

    Ok(to_response(&clean))
}

async fn send_message(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    message: String,
    reply_to: Option<String>,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;

    let tags = match reply_to {
        Some(ref id) => {
            let tag = nostr_sdk::Tag::parse(["e", id])
                .map_err(|e| Response::err(format!("invalid reply-to message ID: {e}")))?;
            Some(vec![tag])
        }
        None => None,
    };

    let result = wn
        .send_message_to_group(&account, &group_id, message, 9, tags)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(to_response(&result))
}

async fn delete_message(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    message_id: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;

    let tag = nostr_sdk::Tag::parse(["e", message_id])
        .map_err(|e| Response::err(format!("invalid message ID: {e}")))?;

    let result = wn
        .send_message_to_group(&account, &group_id, String::new(), 5, Some(vec![tag]))
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(to_response(&result))
}

async fn retry_message(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    event_id_str: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;
    let event_id = nostr_sdk::EventId::from_hex(event_id_str)
        .map_err(|e| Response::err(format!("invalid event ID '{event_id_str}': {e}")))?;

    wn.retry_message_publish(&account, &group_id, &event_id)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(Response::ok(serde_json::json!(null)))
}

async fn react_to_message(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    message_id: &str,
    emoji: String,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;

    let tag = nostr_sdk::Tag::parse(["e", message_id])
        .map_err(|e| Response::err(format!("invalid message ID: {e}")))?;

    let result = wn
        .send_message_to_group(&account, &group_id, emoji, 7, Some(vec![tag]))
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(to_response(&result))
}

async fn unreact_to_message(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    message_id: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;

    // Normalize to canonical lowercase hex before the DB lookup
    let canonical_id = nostr_sdk::EventId::from_hex(message_id)
        .map_err(|e| Response::err(format!("invalid message ID '{message_id}': {e}")))?
        .to_hex();

    // Look up the single target message directly — avoids a full-page scan that would
    // silently fail for messages outside the most-recent page.
    let target_msg = wn
        .fetch_message_by_id(&account.pubkey, &group_id, &canonical_id)
        .await
        .map_err(|e| Response::err(e.to_string()))?
        .ok_or_else(|| Response::err(format!("message not found: {message_id}")))?;

    let user_reaction = target_msg
        .reactions
        .user_reactions
        .iter()
        .find(|r| r.user == account.pubkey)
        .ok_or_else(|| Response::err("no reaction to remove"))?;

    // Send a kind 5 deletion event targeting the reaction event
    let tag = nostr_sdk::Tag::parse(["e", &user_reaction.reaction_id.to_hex()])
        .map_err(|e| Response::err(format!("invalid reaction event ID: {e}")))?;

    let result = wn
        .send_message_to_group(&account, &group_id, String::new(), 5, Some(vec![tag]))
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(to_response(&result))
}

async fn upload_media(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    file_path: &str,
    send: bool,
    message: Option<String>,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;

    let media_file = wn
        .upload_chat_media(&account, &group_id, file_path, None, None)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    if !send {
        return Ok(to_response(&media_file));
    }

    let imeta_tag = build_imeta_tag(&media_file)?;
    let caption = message.unwrap_or_default();

    let msg_result = wn
        .send_message_to_group(&account, &group_id, caption, 9, Some(vec![imeta_tag]))
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(to_response(&serde_json::json!({
        "media": serde_json::to_value(&media_file).map_err(|e| Response::err(e.to_string()))?,
        "message": serde_json::to_value(&msg_result).map_err(|e| Response::err(e.to_string()))?,
    })))
}

/// Build an `imeta` tag from an uploaded `MediaFile` per MIP-04.
fn build_imeta_tag(
    media_file: &crate::whitenoise::database::media_files::MediaFile,
) -> Result<nostr_sdk::Tag, Response> {
    let blossom_url = media_file
        .blossom_url
        .as_deref()
        .ok_or_else(|| Response::err("uploaded media has no blossom URL"))?;

    let original_hash_hex = media_file
        .original_file_hash
        .as_ref()
        .map(hex::encode)
        .ok_or_else(|| Response::err("uploaded media has no original file hash"))?;

    let mut parts: Vec<String> = vec![
        "imeta".to_string(),
        format!("url {}", blossom_url),
        format!("m {}", media_file.mime_type),
        format!("x {}", original_hash_hex),
    ];

    if let Some(ref fm) = media_file.file_metadata {
        if let Some(ref filename) = fm.original_filename {
            parts.push(format!("filename {}", filename));
        }
        if let Some(ref blurhash) = fm.blurhash {
            parts.push(format!("blurhash {}", blurhash));
        }
        if let Some(ref dim) = fm.dimensions {
            parts.push(format!("dim {}", dim));
        }
    }

    if let Some(ref nonce) = media_file.nonce {
        parts.push(format!("n {}", nonce));
    }

    if let Some(ref version) = media_file.scheme_version {
        parts.push(format!("v {}", version));
    }

    let parts_refs: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();
    nostr_sdk::Tag::parse(parts_refs)
        .map_err(|e| Response::err(format!("failed to create imeta tag: {e}")))
}

async fn download_media(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    file_hash_hex: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;

    let hash_bytes: [u8; 32] = hex::decode(file_hash_hex)
        .map_err(|e| Response::err(format!("invalid file hash: {e}")))?
        .try_into()
        .map_err(|v: Vec<u8>| {
            Response::err(format!("file hash must be 32 bytes (got {})", v.len()))
        })?;

    let media_file = wn
        .download_chat_media(&account, &group_id, &hash_bytes)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(to_response(&media_file))
}

async fn list_media(wn: &Whitenoise, group_id_hex: &str) -> Result<Response, Response> {
    let group_id = parse_group_id(group_id_hex)?;

    let files = wn
        .get_media_files_for_group(&group_id)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(to_response(&files))
}

fn parse_group_id(s: &str) -> Result<GroupId, Response> {
    let bytes =
        hex::decode(s).map_err(|e| Response::err(format!("invalid group ID '{s}': {e}")))?;
    if bytes.is_empty() {
        return Err(Response::err("invalid group ID: empty"));
    }
    Ok(GroupId::from_slice(&bytes))
}

fn to_response<T: serde::Serialize>(value: &T) -> Response {
    match serde_json::to_value(value) {
        Ok(v) => Response::ok(v),
        Err(e) => Response::err(format!("serialization error: {e}")),
    }
}

async fn debug_health(wn: &Whitenoise, account_str: &str) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let account_ok = wn
        .is_account_subscriptions_operational(&account)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    let global_ok = wn
        .is_global_subscriptions_operational()
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(Response::ok(serde_json::json!({
        "account_subscriptions": account_ok,
        "discovery_subscriptions": global_ok,
    })))
}

async fn debug_ratchet_tree(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;
    let info = wn
        .ratchet_tree_info(&account, &group_id)
        .map_err(|e| Response::err(e.to_string()))?;

    let leaf_nodes: Vec<serde_json::Value> = info
        .leaf_nodes
        .iter()
        .map(|leaf| {
            serde_json::json!({
                "index": leaf.index,
                "encryption_key": leaf.encryption_key,
                "signature_key": leaf.signature_key,
                "credential_identity": leaf.credential_identity,
            })
        })
        .collect();

    Ok(Response::ok(serde_json::json!({
        "tree_hash": info.tree_hash,
        "serialized_tree": info.serialized_tree,
        "leaf_nodes": leaf_nodes,
    })))
}

async fn promote_admin(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    pubkey_str: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;
    let target_pk = parse_pubkey(pubkey_str)?;

    let members = wn
        .group_members(&account, &group_id)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    if !members.contains(&target_pk) {
        return Err(Response::err("user is not a group member"));
    }

    let mut admins = wn
        .group_admins(&account, &group_id)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    if admins.contains(&target_pk) {
        return Err(Response::err("user is already an admin"));
    }
    admins.push(target_pk);

    // Authorization (caller must be admin) is enforced by update_group_data.
    let update = NostrGroupDataUpdate::new().admins(admins);
    wn.update_group_data(&account, &group_id, update)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(Response::ok(serde_json::json!(null)))
}

async fn demote_admin(
    wn: &Whitenoise,
    account_str: &str,
    group_id_hex: &str,
    pubkey_str: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let group_id = parse_group_id(group_id_hex)?;
    let target_pk = parse_pubkey(pubkey_str)?;

    let admins = wn
        .group_admins(&account, &group_id)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    if !admins.contains(&target_pk) {
        return Err(Response::err("user is not an admin"));
    }
    if admins.len() == 1 {
        return Err(Response::err("cannot demote the last admin"));
    }
    let admins: Vec<_> = admins.into_iter().filter(|pk| pk != &target_pk).collect();

    // Authorization (caller must be admin) is enforced by update_group_data.
    let update = NostrGroupDataUpdate::new().admins(admins);
    wn.update_group_data(&account, &group_id, update)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    Ok(Response::ok(serde_json::json!(null)))
}

async fn keys_list(wn: &Whitenoise, account_str: &str) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let packages = wn
        .fetch_all_key_packages_for_account(&account)
        .await
        .map_err(|e| Response::err(e.to_string()))?;

    let items: Vec<serde_json::Value> = packages
        .iter()
        .map(|event| {
            serde_json::json!({
                "event_id": event.id.to_hex(),
                "created_at": event.created_at.as_secs(),
            })
        })
        .collect();
    Ok(Response::ok(serde_json::json!({ "key_packages": items })))
}

async fn keys_publish(wn: &Whitenoise, account_str: &str) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    wn.publish_key_package_for_account(&account)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(Response::ok(serde_json::json!(null)))
}

async fn keys_delete(
    wn: &Whitenoise,
    account_str: &str,
    event_id_hex: &str,
) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let event_id = nostr_sdk::EventId::from_hex(event_id_hex)
        .map_err(|e| Response::err(format!("invalid event ID: {e}")))?;
    let deleted = wn
        .delete_key_package_for_account(&account, &event_id, true)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(Response::ok(serde_json::json!({ "deleted": deleted })))
}

async fn keys_delete_all(wn: &Whitenoise, account_str: &str) -> Result<Response, Response> {
    let account = find_account(wn, account_str).await?;
    let count = wn
        .delete_all_key_packages_for_account(&account, true)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    Ok(Response::ok(serde_json::json!({ "deleted_count": count })))
}

async fn keys_check(wn: &Whitenoise, pubkey_str: &str) -> Result<Response, Response> {
    let pubkey = parse_pubkey(pubkey_str)?;
    // Side effect: creates a local user record if not already present.
    let user = wn
        .find_or_create_user_by_pubkey(&pubkey, UserSyncMode::Blocking)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    let status = user
        .key_package_status(wn)
        .await
        .map_err(|e| Response::err(e.to_string()))?;
    let result = match status {
        KeyPackageStatus::Valid(event) => serde_json::json!({
            "status": "valid",
            "event_id": event.id.to_hex(),
            "created_at": event.created_at.as_secs(),
        }),
        KeyPackageStatus::NotFound => serde_json::json!({
            "status": "not_found",
        }),
        KeyPackageStatus::Incompatible => serde_json::json!({
            "status": "incompatible",
        }),
    };
    Ok(Response::ok(result))
}

#[cfg(test)]
mod tests {
    use nostr_sdk::ToBech32;

    use super::*;

    // --- parse_pubkey ---

    #[test]
    fn parse_pubkey_valid_hex() {
        let hex = "4dd7a05f5f668589d5d3025a30e3a2603f2d4fe7fb9d0e2b33914b765e9d6f69";
        assert!(parse_pubkey(hex).is_ok());
    }

    #[test]
    fn parse_pubkey_valid_npub() {
        let hex = "4dd7a05f5f668589d5d3025a30e3a2603f2d4fe7fb9d0e2b33914b765e9d6f69";
        let pk = PublicKey::from_hex(hex).unwrap();
        let npub = pk.to_bech32().unwrap();
        assert!(parse_pubkey(&npub).is_ok());
    }

    #[test]
    fn parse_pubkey_invalid() {
        let err = parse_pubkey("not-a-pubkey").unwrap_err();
        assert!(err.error.unwrap().message.contains("not-a-pubkey"));
    }

    // --- parse_group_id ---

    #[test]
    fn parse_group_id_valid_hex() {
        let gid = parse_group_id("abcd1234").unwrap();
        assert_eq!(gid.as_slice(), &[0xab, 0xcd, 0x12, 0x34]);
    }

    #[test]
    fn parse_group_id_empty_string() {
        let err = parse_group_id("").unwrap_err();
        assert!(err.error.unwrap().message.contains("empty"));
    }

    #[test]
    fn parse_group_id_invalid_hex() {
        let err = parse_group_id("zzzz").unwrap_err();
        assert!(err.error.unwrap().message.contains("invalid group ID"));
    }

    #[test]
    fn parse_group_id_odd_length() {
        let err = parse_group_id("abc").unwrap_err();
        assert!(err.error.is_some());
    }

    // --- to_response ---

    #[test]
    fn to_response_serializable_value() {
        let resp = to_response(&serde_json::json!({"key": "value"}));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn to_response_string() {
        let resp = to_response(&"hello");
        assert_eq!(resp.result.unwrap(), serde_json::json!("hello"));
    }

    // --- parse_relay_type ---

    #[test]
    fn parse_relay_type_nip65() {
        assert_eq!(parse_relay_type("nip65").unwrap(), RelayType::Nip65);
    }

    #[test]
    fn parse_relay_type_inbox() {
        assert_eq!(parse_relay_type("inbox").unwrap(), RelayType::Inbox);
    }

    #[test]
    fn parse_relay_type_key_package() {
        assert_eq!(
            parse_relay_type("key_package").unwrap(),
            RelayType::KeyPackage
        );
    }

    #[test]
    fn parse_relay_type_invalid() {
        let err = parse_relay_type("bogus").unwrap_err();
        let msg = err.error.unwrap().message;
        assert!(msg.contains("bogus"));
        assert!(msg.contains("Valid types"));
    }

    // --- parse_relay_url ---

    #[test]
    fn parse_relay_url_valid_wss() {
        let url = parse_relay_url("wss://relay.example.com").unwrap();
        assert_eq!(url.as_str(), "wss://relay.example.com");
    }

    #[test]
    fn parse_relay_url_invalid() {
        let err = parse_relay_url("not-a-url").unwrap_err();
        assert!(err.error.unwrap().message.contains("invalid relay URL"));
    }
}

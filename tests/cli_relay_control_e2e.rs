//! End-to-end tests focused on relay-control plane behavior.
//!
//! Run with:
//! ```sh
//! cargo test --features cli,integration-tests --test cli_relay_control_e2e
//! ```

#![cfg(all(feature = "cli", feature = "integration-tests"))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use whitenoise::cli::client;
use whitenoise::cli::protocol::{Request, Response};
use whitenoise::test_fixtures::nostr::{
    JEFF_PUBKEY_HEX, MAX_PUBKEY_HEX, publish_user_search_seed_events,
};

#[path = "support/cli.rs"]
mod cli_support;

use cli_support::{
    Daemon, bytes_field_hex, group_id_hex, poll_until, wait_for_group, wait_for_message, wn,
};

const LOCAL_DEV_RELAYS: &[&str] = &["ws://localhost:8080", "ws://localhost:7777"];

/// Run a user search and collect all updates until SearchCompleted.
///
/// If `stop_pubkey` is provided, stop early as soon as that pubkey appears in
/// results — this avoids waiting for the full pipeline to drain large graphs.
async fn users_search(
    socket: &Path,
    account: &str,
    query: &str,
    radius_start: u8,
    radius_end: u8,
    stop_pubkey: Option<&str>,
) -> Vec<serde_json::Value> {
    let request = Request::UsersSearch {
        account: account.to_string(),
        query: query.to_string(),
        radius_start,
        radius_end,
    };

    let mut updates = Vec::new();
    let stream_fut = client::stream(socket, &request, |response: &Response| {
        if let Some(error) = &response.error {
            panic!("users search returned error: {}", error.message);
        }
        if let Some(result) = &response.result {
            updates.push(result.clone());

            // Early termination: stop once the target pubkey appears in results
            if let Some(target) = stop_pubkey
                && result["new_results"]
                    .as_array()
                    .is_some_and(|arr| arr.iter().any(|r| r["pubkey"].as_str() == Some(target)))
            {
                return false; // disconnect, cancelling the search
            }
        }
        true
    });

    match tokio::time::timeout(Duration::from_secs(60), stream_fut).await {
        Ok(res) => res.expect("stream users search"),
        Err(_) => panic!(
            "users_search timed out after 60s (query={query:?}, collected {} updates so far)",
            updates.len()
        ),
    }

    updates
}

fn relay_control_state(socket: &PathBuf) -> serde_json::Value {
    wn(socket, &["debug", "relay-control-state"])
}

fn session_has_live_relay(session: &serde_json::Value) -> bool {
    session["relays"].as_array().is_some_and(|relays| {
        relays.iter().any(|relay| {
            matches!(
                relay["status"].as_str(),
                Some("Connected") | Some("Connecting")
            )
        })
    })
}

fn search_updates_include_pubkey(updates: &[serde_json::Value], pubkey_hex: &str) -> bool {
    updates
        .iter()
        .filter_map(|update| update["new_results"].as_array())
        .flatten()
        .any(|result| result["pubkey"].as_str() == Some(pubkey_hex))
}

#[tokio::test]
async fn relay_control_snapshot_tracks_seeded_discovery_graph() {
    publish_user_search_seed_events(LOCAL_DEV_RELAYS)
        .await
        .expect("publish search seed events");

    let alice = Daemon::start().await;
    let alice_pk = wn(&alice.socket, &["create-identity"])["pubkey"]
        .as_str()
        .unwrap()
        .to_string();

    let jeff_updates = users_search(
        &alice.socket,
        &alice_pk,
        "Jeff",
        0,
        2,
        Some(JEFF_PUBKEY_HEX),
    )
    .await;
    assert!(
        search_updates_include_pubkey(&jeff_updates, JEFF_PUBKEY_HEX),
        "searching Jeff should surface the fallback seed user"
    );

    // Max is one of Jeff's follows — found at radius 2 (searcher → fallback → Jeff → Max).
    let max_updates =
        users_search(&alice.socket, &alice_pk, "Max", 0, 3, Some(MAX_PUBKEY_HEX)).await;
    assert!(
        search_updates_include_pubkey(&max_updates, MAX_PUBKEY_HEX),
        "searching Max should traverse Jeff's seeded follow graph"
    );

    // Force the seeded users into the local user graph so discovery re-sync
    // picks them up in the watched-user set.
    let _ = wn(&alice.socket, &["users", "show", JEFF_PUBKEY_HEX]);
    let _ = wn(&alice.socket, &["users", "show", MAX_PUBKEY_HEX]);

    poll_until(
        Duration::from_secs(30),
        "relay-control snapshot never showed a seeded discovery graph",
        || {
            let snapshot = relay_control_state(&alice.socket);

            snapshot["discovery"]["watched_user_count"]
                .as_u64()
                .is_some_and(|count| count >= 3)
                && snapshot["discovery"]["follow_list_subscription_count"].as_u64() == Some(1)
                && snapshot["discovery"]["public_subscription_ids"]
                    .as_array()
                    .is_some_and(|ids| !ids.is_empty())
                && snapshot["discovery"]["follow_list_subscription_ids"]
                    .as_array()
                    .is_some_and(|ids| !ids.is_empty())
                && session_has_live_relay(&snapshot["discovery"]["session"])
                && snapshot["account_inbox"]["accounts"]
                    .as_array()
                    .is_some_and(|accounts| {
                        accounts.iter().any(|account| {
                            account["account_pubkey"].as_str() == Some(alice_pk.as_str())
                                && account["subscription_id"]
                                    .as_str()
                                    .is_some_and(|sub_id| sub_id.ends_with("_giftwrap"))
                                && session_has_live_relay(&account["session"])
                        })
                    })
        },
    )
    .await;
}

#[tokio::test]
async fn relay_control_snapshot_tracks_live_planes() {
    let alice = Daemon::start().await;
    let bob = Daemon::start().await;

    let alice_pk = wn(&alice.socket, &["create-identity"])["pubkey"]
        .as_str()
        .unwrap()
        .to_string();
    let bob_pk = wn(&bob.socket, &["create-identity"])["pubkey"]
        .as_str()
        .unwrap()
        .to_string();

    let group = wn(
        &alice.socket,
        &[
            "--account",
            &alice_pk,
            "groups",
            "create",
            "Relay Control Snapshot",
            &bob_pk,
        ],
    );
    let gid = group_id_hex(&group);
    let nostr_group_id = bytes_field_hex(&group, "nostr_group_id");

    wait_for_group(&bob.socket, &bob_pk).await;
    tokio::time::sleep(Duration::from_secs(5)).await;

    wn(
        &alice.socket,
        &[
            "--account",
            &alice_pk,
            "messages",
            "send",
            &gid,
            "relay control snapshot ping",
        ],
    );
    wait_for_message(&bob.socket, &bob_pk, &gid, "relay control snapshot ping").await;

    poll_until(
        Duration::from_secs(30),
        "Alice relay-control snapshot never showed active discovery/inbox/group planes",
        || {
            let snapshot = relay_control_state(&alice.socket);

            let discovery_ready = snapshot["discovery"]["public_subscription_ids"]
                .as_array()
                .is_some_and(|ids| !ids.is_empty())
                && snapshot["discovery"]["follow_list_subscription_ids"]
                    .as_array()
                    .is_some_and(|ids| !ids.is_empty())
                && session_has_live_relay(&snapshot["discovery"]["session"]);

            let inbox_ready = snapshot["account_inbox"]["accounts"]
                .as_array()
                .is_some_and(|accounts| {
                    accounts.iter().any(|account| {
                        account["account_pubkey"].as_str() == Some(alice_pk.as_str())
                            && account["subscription_id"]
                                .as_str()
                                .is_some_and(|sub_id| sub_id.ends_with("_giftwrap"))
                            && account["relay_count"]
                                .as_u64()
                                .is_some_and(|count| count >= 1)
                            && session_has_live_relay(&account["session"])
                    })
                });

            let group_ready = snapshot["group"]["groups"]
                .as_array()
                .is_some_and(|groups| {
                    groups.iter().any(|group| {
                        group["account_pubkey"].as_str() == Some(alice_pk.as_str())
                            && group["group_id"].as_str() == Some(nostr_group_id.as_str())
                            && group["subscription_id"]
                                .as_str()
                                .is_some_and(|sub_id| sub_id.contains("_mls_messages"))
                            && group["relay_count"]
                                .as_u64()
                                .is_some_and(|count| count >= 1)
                    })
                })
                && session_has_live_relay(&snapshot["group"]["session"]);

            discovery_ready && inbox_ready && group_ready
        },
    )
    .await;

    poll_until(
        Duration::from_secs(30),
        "Bob relay-control snapshot never showed active discovery/inbox/group planes",
        || {
            let snapshot = relay_control_state(&bob.socket);

            let discovery_ready = snapshot["discovery"]["public_subscription_ids"]
                .as_array()
                .is_some_and(|ids| !ids.is_empty())
                && snapshot["discovery"]["follow_list_subscription_ids"]
                    .as_array()
                    .is_some_and(|ids| !ids.is_empty())
                && session_has_live_relay(&snapshot["discovery"]["session"]);

            let inbox_ready = snapshot["account_inbox"]["accounts"]
                .as_array()
                .is_some_and(|accounts| {
                    accounts.iter().any(|account| {
                        account["account_pubkey"].as_str() == Some(bob_pk.as_str())
                            && account["subscription_id"]
                                .as_str()
                                .is_some_and(|sub_id| sub_id.ends_with("_giftwrap"))
                            && account["relay_count"]
                                .as_u64()
                                .is_some_and(|count| count >= 1)
                            && session_has_live_relay(&account["session"])
                    })
                });

            let group_ready = snapshot["group"]["groups"]
                .as_array()
                .is_some_and(|groups| {
                    groups.iter().any(|group| {
                        group["account_pubkey"].as_str() == Some(bob_pk.as_str())
                            && group["group_id"].as_str() == Some(nostr_group_id.as_str())
                            && group["subscription_id"]
                                .as_str()
                                .is_some_and(|sub_id| sub_id.contains("_mls_messages"))
                            && group["relay_count"]
                                .as_u64()
                                .is_some_and(|count| count >= 1)
                    })
                })
                && session_has_live_relay(&snapshot["group"]["session"]);

            discovery_ready && inbox_ready && group_ready
        },
    )
    .await;
}

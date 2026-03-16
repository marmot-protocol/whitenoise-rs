//! End-to-end tests for the CLI daemon.
//!
//! These tests spawn real `wnd` daemon processes with isolated data directories
//! and drive them using the `wn` CLI binary with `--json` output.
//!
//! Run with:
//! ```sh
//! cargo test --features cli,integration-tests --test cli_e2e
//! ```
//!
//! Prerequisites:
//! - Local Nostr relays must be running for messaging tests (same setup as the
//!   integration test binary).

#![cfg(all(feature = "cli", feature = "integration-tests"))]

use std::time::Duration;

#[path = "support/cli.rs"]
mod cli_support;

use cli_support::{Daemon, group_id_hex, wait_for_group, wait_for_message, wn};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_daemons_create_independent_identities() {
    let d1 = Daemon::start().await;
    let d2 = Daemon::start().await;

    let r1 = wn(&d1.socket, &["create-identity"]);
    let r2 = wn(&d2.socket, &["create-identity"]);

    let pk1 = r1["pubkey"].as_str().expect("pubkey");
    let pk2 = r2["pubkey"].as_str().expect("pubkey");
    assert_ne!(pk1, pk2, "two daemons created the same pubkey");

    let a1 = wn(&d1.socket, &["whoami"]);
    let a2 = wn(&d2.socket, &["whoami"]);

    assert_eq!(a1.as_array().unwrap().len(), 1);
    assert_eq!(a2.as_array().unwrap().len(), 1);
}

/// Single-daemon test: identity creation, key export, and profile management.
///
/// No cross-daemon communication needed — exercises local-only operations.
#[tokio::test]
async fn account_profile_and_export() {
    let d = Daemon::start().await;

    let pk = wn(&d.socket, &["create-identity"])["pubkey"]
        .as_str()
        .unwrap()
        .to_string();

    // Export nsec — should be a valid bech32 secret key
    let nsec = wn(&d.socket, &["export-nsec", &pk]);
    let nsec_str = nsec.as_str().expect("nsec should be a string");
    assert!(
        nsec_str.starts_with("nsec1"),
        "expected nsec1 prefix, got: {nsec_str}"
    );

    // Fresh profile — should be a JSON object (metadata fields may be null)
    let profile = wn(&d.socket, &["--account", &pk, "profile", "show"]);
    assert!(profile.is_object(), "profile should be a JSON object");

    // Update profile fields
    wn(
        &d.socket,
        &[
            "--account",
            &pk,
            "profile",
            "update",
            "--name",
            "testuser",
            "--display-name",
            "Test User",
            "--about",
            "CLI test account",
        ],
    );

    // Show reflects the update
    let profile = wn(&d.socket, &["--account", &pk, "profile", "show"]);
    assert_eq!(profile["name"].as_str(), Some("testuser"));
    assert_eq!(profile["display_name"].as_str(), Some("Test User"));
    assert_eq!(profile["about"].as_str(), Some("CLI test account"));
}

/// Follow/unfollow lifecycle across two daemons.
#[tokio::test]
async fn follows_lifecycle() {
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

    // Alice follows Bob
    wn(
        &alice.socket,
        &["--account", &alice_pk, "follows", "add", &bob_pk],
    );

    let check = wn(
        &alice.socket,
        &["--account", &alice_pk, "follows", "check", &bob_pk],
    );
    assert_eq!(check["following"], true);

    // Bob appears in Alice's follows list
    let follows = wn(&alice.socket, &["--account", &alice_pk, "follows", "list"]);
    let pks: Vec<&str> = follows
        .as_array()
        .expect("follows list")
        .iter()
        .filter_map(|f| f["pubkey"].as_str())
        .collect();
    assert!(
        pks.contains(&bob_pk.as_str()),
        "Bob should be in Alice's follows list"
    );

    // Unfollow
    wn(
        &alice.socket,
        &["--account", &alice_pk, "follows", "remove", &bob_pk],
    );

    let check = wn(
        &alice.socket,
        &["--account", &alice_pk, "follows", "check", &bob_pk],
    );
    assert_eq!(check["following"], false);
}

/// Group metadata queries: show, members, admins, rename.
///
/// Creates a group, waits for the second member to join, then verifies
/// the group state from the creator's perspective.
#[tokio::test]
async fn group_metadata_and_membership() {
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
            "Metadata Test",
            &bob_pk,
        ],
    );
    let gid = group_id_hex(&group);

    wait_for_group(&bob.socket, &bob_pk).await;

    // groups show returns the MLS group object
    let detail = wn(
        &alice.socket,
        &["--account", &alice_pk, "groups", "show", &gid],
    );
    assert!(detail.is_object(), "group detail should be a JSON object");
    assert!(
        detail.get("mls_group_id").is_some(),
        "should contain mls_group_id"
    );

    // members includes both Alice and Bob
    let members = wn(
        &alice.socket,
        &["--account", &alice_pk, "groups", "members", &gid],
    );
    let member_pks: Vec<&str> = members
        .as_array()
        .expect("members array")
        .iter()
        .filter_map(|m| m["pubkey"].as_str())
        .collect();
    assert!(
        member_pks.contains(&alice_pk.as_str()),
        "Alice should be a member"
    );
    assert!(
        member_pks.contains(&bob_pk.as_str()),
        "Bob should be a member"
    );

    // admins includes Alice (the creator)
    let admins = wn(
        &alice.socket,
        &["--account", &alice_pk, "groups", "admins", &gid],
    );
    let admin_pks: Vec<&str> = admins
        .as_array()
        .expect("admins array")
        .iter()
        .filter_map(|m| m["pubkey"].as_str())
        .collect();
    assert!(
        admin_pks.contains(&alice_pk.as_str()),
        "Alice should be an admin"
    );

    // Rename succeeds (dispatch returns null on success)
    wn(
        &alice.socket,
        &[
            "--account",
            &alice_pk,
            "groups",
            "rename",
            &gid,
            "Renamed Group",
        ],
    );
}

#[tokio::test]
async fn cross_daemon_group_messaging() {
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
            "E2E Test Group",
            &bob_pk,
        ],
    );
    let gid = group_id_hex(&group);

    wait_for_group(&bob.socket, &bob_pk).await;

    // Allow background welcome finalization to complete (subscription setup,
    // key package rotation, etc.) before sending messages.
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Alice sends, Bob receives
    wn(
        &alice.socket,
        &[
            "--account",
            &alice_pk,
            "messages",
            "send",
            &gid,
            "Hello from Alice",
        ],
    );
    wait_for_message(&bob.socket, &bob_pk, &gid, "Hello from Alice").await;

    // Bob sends, Alice receives
    wn(
        &bob.socket,
        &[
            "--account",
            &bob_pk,
            "messages",
            "send",
            &gid,
            "Hello from Bob",
        ],
    );
    wait_for_message(&alice.socket, &alice_pk, &gid, "Hello from Bob").await;
}

/// Adding a second identity to a daemon must not break the first account's
/// messaging.
///
/// Regression test: creating a new identity used to leave `last_synced_at = NULL`,
/// which poisoned `compute_global_since_timestamp()` for ALL accounts, forcing
/// global subscriptions to use `since=None` (unbounded re-fetch).
#[tokio::test]
async fn second_identity_does_not_break_first_account_messaging() {
    let alice = Daemon::start().await;
    let bob = Daemon::start().await;

    // Account 1 on Alice's daemon
    let alice_pk1 = wn(&alice.socket, &["create-identity"])["pubkey"]
        .as_str()
        .unwrap()
        .to_string();
    let bob_pk = wn(&bob.socket, &["create-identity"])["pubkey"]
        .as_str()
        .unwrap()
        .to_string();

    // Establish a working chat with account 1
    let group = wn(
        &alice.socket,
        &[
            "--account",
            &alice_pk1,
            "groups",
            "create",
            "Multi-Account Test",
            &bob_pk,
        ],
    );
    let gid = group_id_hex(&group);
    wait_for_group(&bob.socket, &bob_pk).await;
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Verify messaging works before adding second account
    wn(
        &alice.socket,
        &[
            "--account",
            &alice_pk1,
            "messages",
            "send",
            &gid,
            "before second identity",
        ],
    );
    wait_for_message(&bob.socket, &bob_pk, &gid, "before second identity").await;

    // Create a SECOND identity on Alice's daemon — this is the critical step
    let alice_pk2 = wn(&alice.socket, &["create-identity"])["pubkey"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(alice_pk1, alice_pk2);

    // Verify Alice's daemon now has 2 accounts
    let accounts = wn(&alice.socket, &["whoami"]);
    assert_eq!(
        accounts.as_array().unwrap().len(),
        2,
        "Alice should have 2 accounts"
    );

    // Allow subscriptions to re-settle after the new account was added
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Account 1 messaging must still work after account 2 was created
    wn(
        &bob.socket,
        &[
            "--account",
            &bob_pk,
            "messages",
            "send",
            &gid,
            "after second identity",
        ],
    );
    wait_for_message(&alice.socket, &alice_pk1, &gid, "after second identity").await;
}

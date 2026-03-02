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

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Daemon harness
// ---------------------------------------------------------------------------

/// A running `wnd` daemon with its own isolated data directory.
///
/// Automatically killed when dropped.
struct Daemon {
    child: Child,
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Daemon {
    /// Start a new daemon in an isolated temp directory.
    ///
    /// Blocks until the daemon is ready to accept CLI commands.
    async fn start() -> Self {
        let dir = tempfile::tempdir().expect("create temp dir");
        let data_dir = dir.path().join("data");
        let logs_dir = dir.path().join("logs");

        let child = Command::new(env!("CARGO_BIN_EXE_wnd"))
            .args(["--data-dir", &data_dir.display().to_string()])
            .args(["--logs-dir", &logs_dir.display().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start wnd");

        // Socket lives at {data_dir}/dev/wnd.sock in debug builds
        let socket = data_dir.join("dev").join("wnd.sock");

        // Wait for daemon to be ready by polling `wn whoami`
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if socket.exists() {
                let output = Command::new(env!("CARGO_BIN_EXE_wn"))
                    .args([
                        "--json",
                        "--socket",
                        &socket.display().to_string(),
                        "whoami",
                    ])
                    .output()
                    .expect("run wn");
                if output.status.success() {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "daemon did not become ready within 30s"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Self {
            child,
            socket,
            _dir: dir,
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// CLI helpers
// ---------------------------------------------------------------------------

/// Run `wn --json --socket <socket> <args...>` and return the `result` field.
///
/// Panics if the command fails or returns an error response.
fn wn(socket: &PathBuf, args: &[&str]) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_wn"))
        .arg("--json")
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run wn {args:?}: {e}"));

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");

    if !output.status.success() {
        panic!(
            "wn {args:?} exited with {}:\n{stdout}\n{stderr}",
            output.status
        );
    }

    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("invalid JSON from wn {args:?}:\n{stdout}"));

    if let Some(err) = parsed.get("error") {
        panic!("wn {args:?} returned error: {err}");
    }

    parsed
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

/// Extract a hex-encoded MLS group ID from a group JSON response.
///
/// The MDK `GroupId` serializes as `{"value": {"vec": [u8, ...]}}`.
fn group_id_hex(group_json: &serde_json::Value) -> String {
    let bytes = group_json
        .get("mls_group_id")
        .and_then(|v| v.get("value"))
        .and_then(|v| v.get("vec"))
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| {
            panic!(
                "cannot extract mls_group_id from: {}",
                serde_json::to_string_pretty(group_json).unwrap_or_default()
            )
        });

    bytes
        .iter()
        .map(|b| format!("{:02x}", b.as_u64().expect("byte value")))
        .collect()
}

// ---------------------------------------------------------------------------
// Polling helpers
// ---------------------------------------------------------------------------

/// Poll until `predicate` returns `true`, checking every second.
///
/// Panics with `timeout_msg` if the deadline is exceeded.
async fn poll_until(timeout: Duration, timeout_msg: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return;
        }
        assert!(Instant::now() < deadline, "{timeout_msg}");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Wait for a daemon to see at least one group (up to 60s).
async fn wait_for_group(socket: &PathBuf, pubkey: &str) {
    poll_until(
        Duration::from_secs(60),
        "did not see any groups within 60s",
        || {
            wn(socket, &["--account", pubkey, "groups", "list"])
                .as_array()
                .is_some_and(|g| !g.is_empty())
        },
    )
    .await;
}

/// Wait for a specific message content to appear in a group (up to 30s).
async fn wait_for_message(socket: &PathBuf, pubkey: &str, group_id: &str, content: &str) {
    let msg = format!("did not receive message \"{content}\" within 30s");
    poll_until(Duration::from_secs(30), &msg, || {
        wn(socket, &["--account", pubkey, "messages", "list", group_id])
            .as_array()
            .is_some_and(|m| m.iter().any(|msg| msg["content"].as_str() == Some(content)))
    })
    .await;
}

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

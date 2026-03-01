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
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_daemons_create_independent_identities() {
    let d1 = Daemon::start().await;
    let d2 = Daemon::start().await;

    // Create identity on each daemon
    let r1 = wn(&d1.socket, &["create-identity"]);
    let r2 = wn(&d2.socket, &["create-identity"]);

    let pk1 = r1["pubkey"].as_str().expect("pubkey");
    let pk2 = r2["pubkey"].as_str().expect("pubkey");
    assert_ne!(pk1, pk2, "two daemons created the same pubkey");

    // Each daemon should only see its own account
    let a1 = wn(&d1.socket, &["whoami"]);
    let a2 = wn(&d2.socket, &["whoami"]);

    let list1 = a1.as_array().expect("accounts array");
    let list2 = a2.as_array().expect("accounts array");

    assert_eq!(list1.len(), 1, "daemon 1 should have exactly 1 account");
    assert_eq!(list2.len(), 1, "daemon 2 should have exactly 1 account");
}

#[tokio::test]
async fn cross_daemon_group_messaging() {
    let alice = Daemon::start().await;
    let bob = Daemon::start().await;

    // Create identities
    let alice_pk = wn(&alice.socket, &["create-identity"])["pubkey"]
        .as_str()
        .unwrap()
        .to_string();
    let bob_pk = wn(&bob.socket, &["create-identity"])["pubkey"]
        .as_str()
        .unwrap()
        .to_string();

    // Alice creates a group with Bob as a member
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

    // Wait for Bob to receive the MLS welcome and see the group
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let groups = wn(&bob.socket, &["--account", &bob_pk, "groups", "list"]);
        if groups.as_array().is_some_and(|g| !g.is_empty()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Bob did not join the group within 60s"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Allow background welcome finalization to complete (subscription setup,
    // key package rotation, etc.) before sending messages.
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Alice sends a message
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
    // Wait for Bob to see Alice's message
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let msgs = wn(
            &bob.socket,
            &["--account", &bob_pk, "messages", "list", &gid],
        );
        if msgs.as_array().is_some_and(|m| {
            m.iter()
                .any(|msg| msg["content"].as_str() == Some("Hello from Alice"))
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Bob did not receive Alice's message within 30s"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Bob sends a message
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

    // Wait for Alice to see Bob's message
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let msgs = wn(
            &alice.socket,
            &["--account", &alice_pk, "messages", "list", &gid],
        );
        if msgs.as_array().is_some_and(|m| {
            m.iter()
                .any(|msg| msg["content"].as_str() == Some("Hello from Bob"))
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Alice did not receive Bob's message within 30s"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

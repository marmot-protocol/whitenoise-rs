use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A running `wnd` daemon with its own isolated data directory.
///
/// Automatically killed when dropped.
pub(crate) struct Daemon {
    child: Child,
    pub(crate) socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Daemon {
    /// Start a new daemon in an isolated temp directory.
    ///
    /// Blocks until the daemon is ready to accept CLI commands.
    pub(crate) async fn start() -> Self {
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

        let socket = data_dir.join("dev").join("wnd.sock");

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

/// Run `wn --json --socket <socket> <args...>` and return the `result` field.
///
/// Panics if the command fails or returns an error response.
pub(crate) fn wn(socket: &PathBuf, args: &[&str]) -> serde_json::Value {
    wn_try(socket, args).unwrap_or_else(|error| panic!("{error}"))
}

/// Run `wn --json --socket <socket> <args...>` and return the `result` field.
///
/// Returns a stringified error instead of panicking.
pub(crate) fn wn_try(socket: &PathBuf, args: &[&str]) -> Result<serde_json::Value, String> {
    let output = Command::new(env!("CARGO_BIN_EXE_wn"))
        .arg("--json")
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run wn {args:?}: {e}"))?;

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");

    if !output.status.success() {
        return Err(format!(
            "wn {args:?} exited with {}:\n{stdout}\n{stderr}",
            output.status
        ));
    }

    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|_| format!("invalid JSON from wn {args:?}:\n{stdout}"))?;

    if let Some(err) = parsed.get("error") {
        return Err(format!("wn {args:?} returned error: {err}"));
    }

    Ok(parsed
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

/// Extract a hex-encoded MLS group ID from a group JSON response.
///
/// The MDK `GroupId` serializes as `{"value": {"vec": [u8, ...]}}`.
pub(crate) fn group_id_hex(group_json: &serde_json::Value) -> String {
    bytes_field_hex(group_json, "mls_group_id")
}

/// Extract a hex-encoded byte field from a JSON response.
pub(crate) fn bytes_field_hex(group_json: &serde_json::Value, field_name: &str) -> String {
    let bytes = group_json
        .get(field_name)
        .and_then(|v| match v {
            serde_json::Value::Object(_) => v.get("value").and_then(|v| v.get("vec")),
            _ => Some(v),
        })
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| {
            panic!(
                "cannot extract {field_name} from: {}",
                serde_json::to_string_pretty(group_json).unwrap_or_default()
            )
        });

    bytes
        .iter()
        .map(|b| format!("{:02x}", b.as_u64().expect("byte value")))
        .collect()
}

/// Poll until `predicate` returns `true`, checking every second.
///
/// Panics with `timeout_msg` if the deadline is exceeded.
pub(crate) async fn poll_until(
    timeout: Duration,
    timeout_msg: &str,
    mut predicate: impl FnMut() -> bool,
) {
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
pub(crate) async fn wait_for_group(socket: &PathBuf, pubkey: &str) {
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
pub(crate) async fn wait_for_message(
    socket: &PathBuf,
    pubkey: &str,
    group_id: &str,
    content: &str,
) {
    let msg = format!("did not receive message \"{content}\" within 30s");
    poll_until(Duration::from_secs(30), &msg, || {
        wn(socket, &["--account", pubkey, "messages", "list", group_id])
            .as_array()
            .is_some_and(|m| m.iter().any(|msg| msg["content"].as_str() == Some(content)))
    })
    .await;
}

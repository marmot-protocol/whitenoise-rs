use std::fs;
use std::path::Path;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::Whitenoise;

use super::config::Config;
use super::dispatch;
use super::protocol::{Request, Response};

/// Start the daemon: bind the socket, accept connections, dispatch requests.
///
/// Returns when a shutdown signal is received or the listener fails.
pub async fn run(config: &Config) -> anyhow::Result<()> {
    let socket_path = config.socket_path();
    let pid_path = config.pid_path();

    // Ensure the parent directory exists
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)?;
    }

    clean_stale_socket(&socket_path, &pid_path)?;

    let listener = UnixListener::bind(&socket_path)?;
    set_socket_permissions(&socket_path)?;
    write_pid_file(&pid_path)?;

    tracing::info!("daemon listening on {}", socket_path.display());

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _addr)) => {
                        tokio::spawn(handle_connection(stream));
                    }
                    Err(e) => {
                        tracing::error!("accept error: {e}");
                    }
                }
            }
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received");
                break;
            }
        }
    }

    // Cleanup
    let wn = Whitenoise::get_instance()?;
    wn.shutdown().await?;
    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(&pid_path);
    tracing::info!("daemon stopped");
    Ok(())
}

/// Handle a single client connection: read one request line, dispatch, respond.
async fn handle_connection(stream: tokio::net::UnixStream) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let response = match lines.next_line().await {
        Ok(Some(line)) => match serde_json::from_str::<Request>(&line) {
            Ok(req) => dispatch::dispatch(req).await,
            Err(e) => Response::err(format!("invalid request: {e}")),
        },
        Ok(None) => return, // Client disconnected without sending
        Err(e) => Response::err(format!("read error: {e}")),
    };

    let mut buf = serde_json::to_vec(&response).unwrap_or_default();
    buf.push(b'\n');
    let _ = writer.write_all(&buf).await;
}

/// If a socket file exists but the process that created it is dead, remove it.
fn clean_stale_socket(socket_path: &Path, pid_path: &Path) -> anyhow::Result<()> {
    if !socket_path.exists() {
        return Ok(());
    }

    if let Some(pid) = read_pid(pid_path).filter(|&p| is_process_alive(p)) {
        anyhow::bail!(
            "daemon already running (pid {pid}). \
             Stop it with: wn daemon stop"
        );
    }

    // Stale socket — previous process died without cleanup
    tracing::info!("removing stale socket at {}", socket_path.display());
    let _ = fs::remove_file(socket_path);
    let _ = fs::remove_file(pid_path);
    Ok(())
}

fn write_pid_file(path: &Path) -> anyhow::Result<()> {
    fs::write(path, std::process::id().to_string())?;
    Ok(())
}

#[cfg(unix)]
fn set_socket_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_socket_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

fn is_process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn send_sigterm(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Read the PID from a pidfile. Returns `None` if the file doesn't exist or is invalid.
pub fn read_pid(pid_path: &Path) -> Option<u32> {
    fs::read_to_string(pid_path).ok()?.trim().parse().ok()
}

/// Check whether the daemon is running by probing the PID file.
pub fn is_daemon_running(config: &Config) -> Option<u32> {
    let pid = read_pid(&config.pid_path())?;
    if is_process_alive(pid) {
        Some(pid)
    } else {
        None
    }
}

/// Stop the daemon by sending SIGTERM to the PID in the pidfile.
pub fn stop_daemon(config: &Config) -> anyhow::Result<()> {
    match is_daemon_running(config) {
        Some(pid) => {
            if !send_sigterm(pid) {
                anyhow::bail!("failed to send SIGTERM to pid {pid}");
            }
            println!("daemon stopped (pid {pid})");
            Ok(())
        }
        None => {
            anyhow::bail!("daemon not running");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_pid_valid() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("test.pid");
        fs::write(&pid_file, "12345").unwrap();
        assert_eq!(read_pid(&pid_file), Some(12345));
    }

    #[test]
    fn read_pid_with_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("test.pid");
        fs::write(&pid_file, "12345\n").unwrap();
        assert_eq!(read_pid(&pid_file), Some(12345));
    }

    #[test]
    fn read_pid_missing_file() {
        assert_eq!(read_pid(Path::new("/nonexistent/path/pid")), None);
    }

    #[test]
    fn read_pid_garbage_content() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("test.pid");
        fs::write(&pid_file, "not-a-number").unwrap();
        assert_eq!(read_pid(&pid_file), None);
    }

    #[test]
    fn read_pid_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("test.pid");
        fs::write(&pid_file, "").unwrap();
        assert_eq!(read_pid(&pid_file), None);
    }

    #[test]
    fn clean_stale_socket_no_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("wnd.sock");
        let pid = dir.path().join("wnd.pid");
        // No socket file exists — should succeed immediately
        assert!(clean_stale_socket(&socket, &pid).is_ok());
    }

    #[test]
    fn clean_stale_socket_removes_stale_files() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("wnd.sock");
        let pid_file = dir.path().join("wnd.pid");

        // Create a socket file and pid file with a definitely-dead PID
        fs::write(&socket, "").unwrap();
        fs::write(&pid_file, "999999999").unwrap();

        assert!(clean_stale_socket(&socket, &pid_file).is_ok());
        assert!(!socket.exists());
        assert!(!pid_file.exists());
    }

    #[test]
    fn clean_stale_socket_errors_if_alive() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("wnd.sock");
        let pid_file = dir.path().join("wnd.pid");

        // Use our own PID — guaranteed to be alive
        fs::write(&socket, "").unwrap();
        fs::write(&pid_file, std::process::id().to_string()).unwrap();

        let result = clean_stale_socket(&socket, &pid_file);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already running"));
    }

    #[test]
    fn clean_stale_socket_removes_when_no_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("wnd.sock");
        let pid_file = dir.path().join("wnd.pid");

        // Socket exists but no PID file — treat as stale
        fs::write(&socket, "").unwrap();

        assert!(clean_stale_socket(&socket, &pid_file).is_ok());
        assert!(!socket.exists());
    }
}

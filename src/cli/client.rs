use std::path::Path;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::protocol::{Request, Response};

/// Send a single request to the daemon and return a response.
pub async fn send(socket_path: &Path, request: &Request) -> anyhow::Result<Response> {
    let stream = UnixStream::connect(socket_path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::ConnectionRefused
            || e.kind() == std::io::ErrorKind::NotFound
        {
            anyhow::anyhow!(
                "daemon not running\n\n  Start it with: wn daemon start\n  Check status:  wn daemon status"
            )
        } else {
            anyhow::anyhow!("failed to connect to daemon: {e}")
        }
    })?;

    let (reader, mut writer) = stream.into_split();

    let mut buf = serde_json::to_vec(request)?;
    buf.push(b'\n');
    writer.write_all(&buf).await?;
    writer.shutdown().await?;

    let mut lines = BufReader::new(reader).lines();
    match lines.next_line().await? {
        Some(line) => Ok(serde_json::from_str(&line)?),
        None => anyhow::bail!("daemon closed connection without responding"),
    }
}

/// Send a streaming request and call the handler for each response line.
///
/// The handler receives each `Response` and returns `true` to continue or `false` to stop.
/// The loop ends when the server sends `stream_end: true`, the handler returns `false`,
/// or the connection closes.
pub async fn stream(
    socket_path: &Path,
    request: &Request,
    mut handler: impl FnMut(&Response) -> bool,
) -> anyhow::Result<()> {
    let stream = UnixStream::connect(socket_path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::ConnectionRefused
            || e.kind() == std::io::ErrorKind::NotFound
        {
            anyhow::anyhow!(
                "daemon not running\n\n  Start it with: wn daemon start\n  Check status:  wn daemon status"
            )
        } else {
            anyhow::anyhow!("failed to connect to daemon: {e}")
        }
    })?;

    let (reader, mut writer) = stream.into_split();

    let mut buf = serde_json::to_vec(request)?;
    buf.push(b'\n');
    writer.write_all(&buf).await?;
    // Don't shutdown the writer — keep the connection open for streaming
    drop(writer);

    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let resp: Response = serde_json::from_str(&line)?;
        if resp.stream_end {
            break;
        }
        if !handler(&resp) {
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// Spin up a minimal server that echoes back a fixed response for any request.
    async fn echo_server(socket_path: &Path, response: Response) {
        let listener = UnixListener::bind(socket_path).unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // Read one request line (we don't care about the content)
        let _request = lines.next_line().await.unwrap();

        // Write the canned response
        let mut buf = serde_json::to_vec(&response).unwrap();
        buf.push(b'\n');
        writer.write_all(&buf).await.unwrap();
    }

    #[tokio::test]
    async fn client_server_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("test.sock");

        let canned = Response::ok(serde_json::json!("pong"));
        let socket_clone = socket.clone();
        let server = tokio::spawn(async move {
            echo_server(&socket_clone, canned).await;
        });

        // Give the server a moment to bind
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let resp = send(&socket, &Request::Ping).await.unwrap();
        assert_eq!(resp.result.unwrap(), serde_json::json!("pong"));
        assert!(resp.error.is_none());

        server.await.unwrap();
    }

    #[tokio::test]
    async fn client_gets_error_response() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("test.sock");

        let canned = Response::err("something broke");
        let socket_clone = socket.clone();
        let server = tokio::spawn(async move {
            echo_server(&socket_clone, canned).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let resp = send(&socket, &Request::Ping).await.unwrap();
        assert!(resp.result.is_none());
        assert_eq!(resp.error.unwrap().message, "something broke");

        server.await.unwrap();
    }

    #[tokio::test]
    async fn client_reports_missing_daemon() {
        let err = send(Path::new("/tmp/nonexistent-wn.sock"), &Request::Ping)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("daemon not running"));
    }
}

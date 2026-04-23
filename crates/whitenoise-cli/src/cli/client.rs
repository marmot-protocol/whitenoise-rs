use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::error::CliError;
use super::protocol::{Request, Response};

fn connection_error(err: std::io::Error) -> CliError {
    if matches!(
        err.kind(),
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
    ) {
        CliError::DaemonUnavailable(
            "daemon not running\n\n  Start it with: wn daemon start\n  Check status:  wn daemon status"
                .to_string(),
        )
    } else {
        CliError::msg(format!("failed to connect to daemon: {err}"))
    }
}

/// Send a single request to the daemon and return a response.
pub async fn send(socket_path: &Path, request: &Request) -> crate::cli::Result<Response> {
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(connection_error)?;

    let (reader, mut writer) = stream.into_split();

    let mut buf = serde_json::to_vec(request)?;
    buf.push(b'\n');
    writer.write_all(&buf).await?;
    writer.shutdown().await?;

    let mut lines = BufReader::new(reader).lines();
    match tokio::time::timeout(Duration::from_secs(30), lines.next_line()).await {
        Ok(Ok(Some(line))) => Ok(serde_json::from_str(&line)?),
        Ok(Ok(None)) => Err(CliError::msg("daemon closed connection without responding")),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(CliError::msg("daemon did not respond within 30s")),
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
) -> crate::cli::Result<()> {
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(connection_error)?;

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

    // --- send timeout ---

    /// Server that accepts a connection but never responds.
    async fn silent_server(socket_path: &Path) {
        let listener = UnixListener::bind(socket_path).unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, _writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        // Read the request but never reply
        let _request = lines.next_line().await.unwrap();
        // Hold the connection open until the test completes
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }

    #[tokio::test]
    async fn send_times_out_when_daemon_hangs() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("test.sock");

        let socket_clone = socket.clone();
        let _server = tokio::spawn(async move {
            silent_server(&socket_clone).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Use a short timeout for the test by wrapping in our own timeout
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(35),
            send(&socket, &Request::Ping),
        )
        .await;

        let err = result.expect("test timeout").unwrap_err();
        assert!(err.to_string().contains("did not respond within 30s"));
    }

    // --- stream tests ---

    /// Server that writes multiple response lines then a stream_end marker.
    async fn streaming_server(socket_path: &Path, responses: Vec<Response>) {
        let listener = UnixListener::bind(socket_path).unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let _request = lines.next_line().await.unwrap();

        for resp in &responses {
            let mut buf = serde_json::to_vec(resp).unwrap();
            buf.push(b'\n');
            writer.write_all(&buf).await.unwrap();
        }

        // Send stream_end
        let end = Response {
            result: None,
            error: None,
            stream_end: true,
        };
        let mut buf = serde_json::to_vec(&end).unwrap();
        buf.push(b'\n');
        writer.write_all(&buf).await.unwrap();
    }

    #[tokio::test]
    async fn stream_receives_multiple_responses() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("test.sock");

        let responses = vec![
            Response::ok(serde_json::json!(1)),
            Response::ok(serde_json::json!(2)),
            Response::ok(serde_json::json!(3)),
        ];
        let socket_clone = socket.clone();
        let server = tokio::spawn(async move {
            streaming_server(&socket_clone, responses).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut received = Vec::new();
        stream(&socket, &Request::Ping, |resp| {
            received.push(resp.result.clone());
            true
        })
        .await
        .unwrap();

        assert_eq!(received.len(), 3);
        assert_eq!(received[0], Some(serde_json::json!(1)));
        assert_eq!(received[2], Some(serde_json::json!(3)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stream_handler_can_stop_early() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("test.sock");

        let responses = vec![
            Response::ok(serde_json::json!("first")),
            Response::ok(serde_json::json!("second")),
            Response::ok(serde_json::json!("third")),
        ];
        let socket_clone = socket.clone();
        let _server = tokio::spawn(async move {
            streaming_server(&socket_clone, responses).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut count = 0;
        stream(&socket, &Request::Ping, |_resp| {
            count += 1;
            count < 2 // stop after receiving 2
        })
        .await
        .unwrap();

        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn stream_handles_connection_close() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("test.sock");

        // Server sends one response then closes (no stream_end)
        let socket_clone = socket.clone();
        let server = tokio::spawn(async move {
            let listener = UnixListener::bind(&socket_clone).unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let _request = lines.next_line().await.unwrap();

            let resp = Response::ok(serde_json::json!("only-one"));
            let mut buf = serde_json::to_vec(&resp).unwrap();
            buf.push(b'\n');
            writer.write_all(&buf).await.unwrap();
            // Drop writer — connection closes
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut received = Vec::new();
        stream(&socket, &Request::Ping, |resp| {
            received.push(resp.result.clone());
            true
        })
        .await
        .unwrap();

        assert_eq!(received.len(), 1);
        server.await.unwrap();
    }
}

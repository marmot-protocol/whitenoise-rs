use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::whitenoise::error::WhitenoiseError;

const CONNECTION_TIMEOUT: Duration = Duration::from_millis(800);
const RETRY_WINDOW: Duration = Duration::from_secs(5);
const RETRY_SLEEP: Duration = Duration::from_millis(200);

#[derive(Clone, Copy)]
enum EndpointCheck {
    TcpReachable,
    WebSocketUpgrade,
}

/// Checks that all required local test services are ready within a bounded retry window.
///
/// Relay endpoints must accept a websocket upgrade, while auxiliary service endpoints
/// only need to accept a TCP connection.
///
/// Returns `Err(WhitenoiseError::Configuration)` listing every failing endpoint and
/// a hint to start Docker services.
pub(crate) async fn assert_test_endpoints_reachable(
    relay_endpoints: &[&str],
    tcp_only_endpoints: &[&str],
    hint: &str,
) -> Result<(), WhitenoiseError> {
    let mut unreachable = Vec::new();

    for endpoint in relay_endpoints {
        if let Some(last_error) = wait_for_endpoint(endpoint, EndpointCheck::WebSocketUpgrade).await
        {
            unreachable.push(format!(
                "{endpoint} (websocket upgrade failed: {last_error})"
            ));
        }
    }

    for endpoint in tcp_only_endpoints {
        if let Some(last_error) = wait_for_endpoint(endpoint, EndpointCheck::TcpReachable).await {
            unreachable.push(format!("{endpoint} (tcp connect failed: {last_error})"));
        }
    }

    if unreachable.is_empty() {
        return Ok(());
    }

    Err(WhitenoiseError::Configuration(format!(
        "Local test services are not reachable: {}. {}",
        unreachable.join(", "),
        hint
    )))
}

async fn wait_for_endpoint(endpoint: &str, check: EndpointCheck) -> Option<String> {
    let deadline = Instant::now() + RETRY_WINDOW;

    loop {
        let result = match check {
            EndpointCheck::TcpReachable => tcp_connect_check(endpoint).await,
            EndpointCheck::WebSocketUpgrade => websocket_upgrade_check(endpoint).await,
        };

        match result {
            Ok(()) => return None,
            Err(err) => {
                if Instant::now() >= deadline {
                    return Some(err);
                }
            }
        }

        tokio::time::sleep(RETRY_SLEEP).await;
    }
}

async fn tcp_connect_check(endpoint: &str) -> Result<(), String> {
    let result = tokio::time::timeout(CONNECTION_TIMEOUT, TcpStream::connect(endpoint)).await;
    match result {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Err("timed out".to_string()),
    }
}

async fn websocket_upgrade_check(endpoint: &str) -> Result<(), String> {
    let handshake = async {
        let mut stream = TcpStream::connect(endpoint)
            .await
            .map_err(|err| err.to_string())?;
        let request = format!(
            "GET / HTTP/1.1\r\nHost: {endpoint}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Protocol: nostr\r\n\r\n"
        );

        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|err| err.to_string())?;

        let mut response = Vec::new();
        let mut chunk = [0_u8; 512];
        loop {
            let bytes_read = stream
                .read(&mut chunk)
                .await
                .map_err(|err| err.to_string())?;
            if bytes_read == 0 {
                break;
            }

            response.extend_from_slice(&chunk[..bytes_read]);

            if response.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }

            if response.len() > 8192 {
                break;
            }
        }

        validate_upgrade_response(&response)
    };

    match tokio::time::timeout(CONNECTION_TIMEOUT, handshake).await {
        Ok(result) => result,
        Err(_) => Err("timed out".to_string()),
    }
}

fn validate_upgrade_response(response: &[u8]) -> Result<(), String> {
    let response_text = String::from_utf8_lossy(response);
    let normalized = response_text.replace("\r\n", "\n");
    let mut lines = normalized.lines();
    let status_line = lines
        .next()
        .ok_or_else(|| "empty HTTP response".to_string())?
        .trim()
        .to_string();

    let status_ok =
        status_line.starts_with("HTTP/1.1 101") || status_line.starts_with("HTTP/1.0 101");
    if !status_ok {
        return Err(format!("unexpected status line: {status_line}"));
    }

    let has_upgrade_header = normalized
        .to_ascii_lowercase()
        .contains("upgrade: websocket");
    if !has_upgrade_header {
        return Err("missing Upgrade: websocket header".to_string());
    }

    Ok(())
}

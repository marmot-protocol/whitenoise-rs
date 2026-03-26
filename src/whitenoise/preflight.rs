use std::time::{Duration, Instant};

use tokio::net::TcpStream;

use crate::whitenoise::error::WhitenoiseError;

const CONNECTION_TIMEOUT: Duration = Duration::from_millis(800);
const RETRY_WINDOW: Duration = Duration::from_secs(5);
const RETRY_SLEEP: Duration = Duration::from_millis(200);

/// Checks that all given TCP endpoints are reachable within a bounded retry window.
///
/// Returns `Err(WhitenoiseError::Configuration)` listing every unreachable endpoint
/// and a hint to start Docker services.
pub(crate) async fn assert_test_endpoints_reachable(
    endpoints: &[&str],
    hint: &str,
) -> Result<(), WhitenoiseError> {
    let mut unreachable = Vec::new();

    for endpoint in endpoints {
        let deadline = Instant::now() + RETRY_WINDOW;
        let mut last_error = "timed out".to_string();
        let mut connected = false;

        loop {
            let result =
                tokio::time::timeout(CONNECTION_TIMEOUT, TcpStream::connect(endpoint)).await;
            match result {
                Ok(Ok(_)) => {
                    connected = true;
                    break;
                }
                Ok(Err(err)) => last_error = err.to_string(),
                Err(_) => last_error = "timed out".to_string(),
            }

            if Instant::now() >= deadline {
                break;
            }

            tokio::time::sleep(RETRY_SLEEP).await;
        }

        if !connected {
            unreachable.push(format!("{endpoint} ({last_error})"));
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

use crate::WhitenoiseError;
use crate::whitenoise::preflight::assert_test_endpoints_reachable;

const REQUIRED_RELAYS: &[&str] = &["127.0.0.1:8080", "127.0.0.1:7777"];
const REQUIRED_TCP_ENDPOINTS: &[&str] = &["127.0.0.1:3000"];
const HINT: &str =
    "Start Docker services with `just docker-up` before running integration tests or benchmarks.";

/// Checks that all local Docker test services are reachable before running tests.
pub async fn ensure_local_test_services_running() -> Result<(), WhitenoiseError> {
    assert_test_endpoints_reachable(REQUIRED_RELAYS, REQUIRED_TCP_ENDPOINTS, HINT).await
}

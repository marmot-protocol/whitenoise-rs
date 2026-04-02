#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

TRANSPONDER_BASE_URL="${TRANSPONDER_BASE_URL:-http://127.0.0.1:8081}"
MAX_ATTEMPTS="${MAX_ATTEMPTS:-20}"
WAIT_INTERVAL="${WAIT_INTERVAL:-1}"

wait_for_http_endpoint() {
    local url=$1
    local label=$2
    local attempts=0

    echo "Waiting for $label at $url"

    while [ "$attempts" -lt "$MAX_ATTEMPTS" ]; do
        attempts=$((attempts + 1))

        if curl --fail --silent --show-error "$url" >/dev/null; then
            echo "✓ $label is ready (attempt $attempts)"
            return 0
        fi

        if [ "$attempts" -le 2 ] || [ $((attempts % 5)) -eq 0 ]; then
            echo "  Attempt $attempts/$MAX_ATTEMPTS: $label not ready"
        fi

        sleep "$WAIT_INTERVAL"
    done

    echo "✗ $label failed to become ready after $MAX_ATTEMPTS attempts" >&2
    return 1
}

check_http_endpoint() {
    local url=$1
    local label=$2

    echo "Checking $label at $url"

    if curl --fail --silent --show-error "$url" >/dev/null; then
        echo "✓ $label is responding"
        return 0
    fi

    echo "✗ $label is not responding after service startup" >&2
    return 1
}

echo "Waiting for local docker services to be ready..."
"$REPO_ROOT/scripts/wait_for_relays.sh"
wait_for_http_endpoint "$TRANSPONDER_BASE_URL/health" "Transponder health endpoint"
check_http_endpoint "$TRANSPONDER_BASE_URL/metrics" "Transponder metrics endpoint"

echo "Note: Transponder /ready is expected to stay not-ready in this stack because APNs and FCM are disabled."
echo "✓ All local docker services are ready!"

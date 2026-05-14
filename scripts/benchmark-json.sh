#!/usr/bin/env bash
#
# benchmark-json.sh — Run a benchmark scenario through `benchmark_test` with
# JSON output, optionally routed through toxiproxy with simulated WAN latency.
#
# Usage:
#   ./scripts/benchmark-json.sh [scenario] [latency_ms]
#     scenario     — empty means run all registered scenarios
#     latency_ms   — 0 (default) means no toxiproxy; >0 brings up the latency
#                    profile and applies that latency per direction on both
#                    relay proxies before running the scenario.
#
# Output: ./benchmark_results/result_<timestamp>[_scenario][_wanNms].json
#
# Examples:
#   ./scripts/benchmark-json.sh                            # all scenarios, fast localhost
#   ./scripts/benchmark-json.sh message-aggregation       # one scenario, fast localhost
#   ./scripts/benchmark-json.sh message-aggregation 100   # one scenario, 100 ms WAN

set -euo pipefail

source "$(dirname "$0")/toxiproxy-helpers.sh"

SCENARIO="${1:-}"
LATENCY_MS="${2:-0}"
DATA_DIR="./dev/data/benchmark_test"
RESULTS_DIR="./benchmark_results"

mkdir -p "$RESULTS_DIR"
rm -rf "$DATA_DIR" && mkdir -p "$DATA_DIR"

TIMESTAMP=$(date +%Y%m%d_%H%M%S)
SUFFIX=""
[ -n "$SCENARIO" ] && SUFFIX="_${SCENARIO}"
[ "$LATENCY_MS" -gt 0 ] && SUFFIX="${SUFFIX}_wan${LATENCY_MS}ms"
OUTPUT="${RESULTS_DIR}/result_${TIMESTAMP}${SUFFIX}.json"

CARGO_BIN=(cargo run --release --features benchmark-tests --bin benchmark_test --)
RELAY_ARGS=()

if [ "$LATENCY_MS" -gt 0 ]; then
    trap toxiproxy_down EXIT
    echo "=== Bringing up toxiproxy at ${LATENCY_MS}ms latency ==="
    toxiproxy_up
    toxiproxy_apply_latency "$LATENCY_MS"
    RELAY_ARGS=(--relays "${TOXIPROXY_RELAYS[0]}" --relays "${TOXIPROXY_RELAYS[1]}")
fi

ARGS=(
    --data-dir "$DATA_DIR"
    --logs-dir "${DATA_DIR}/logs"
    --output-json "$OUTPUT"
)
[ ${#RELAY_ARGS[@]} -gt 0 ] && ARGS+=("${RELAY_ARGS[@]}")
[ -n "$SCENARIO" ] && ARGS+=("$SCENARIO")

RUST_LOG=warn,benchmark_test=info,whitenoise=info \
    "${CARGO_BIN[@]}" "${ARGS[@]}"

rm -rf "$DATA_DIR"
echo "Wrote: $OUTPUT"

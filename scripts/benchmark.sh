#!/usr/bin/env bash
#
# benchmark.sh — Run a benchmark scenario through `benchmark_test`, optionally
# routed through toxiproxy with simulated WAN latency.
#
# Usage:
#   ./scripts/benchmark.sh [scenario] [latency_ms]
#     scenario     — empty means run all registered scenarios
#     latency_ms   — 0 (default) means no toxiproxy; >0 brings up the latency
#                    profile and applies that latency per direction on both
#                    relay proxies before running the scenario.
#
# Examples:
#   ./scripts/benchmark.sh                            # all scenarios, fast localhost
#   ./scripts/benchmark.sh message-aggregation       # one scenario, fast localhost
#   ./scripts/benchmark.sh message-aggregation 100   # one scenario, 100 ms WAN

set -euo pipefail

source "$(dirname "$0")/toxiproxy-helpers.sh"

SCENARIO="${1:-}"
LATENCY_MS="${2:-0}"
DATA_DIR="./dev/data/benchmark_test"

CARGO_BIN=(cargo run --bin benchmark_test --features benchmark-tests --release --)
RELAY_ARGS=()

if [ "$LATENCY_MS" -gt 0 ]; then
    trap toxiproxy_down EXIT
    echo "=== Bringing up toxiproxy at ${LATENCY_MS}ms latency ==="
    toxiproxy_up
    toxiproxy_apply_latency "$LATENCY_MS"
    RELAY_ARGS=(--relays "${TOXIPROXY_RELAYS[0]}" --relays "${TOXIPROXY_RELAYS[1]}")
fi

rm -rf "$DATA_DIR"

ARGS=(--data-dir "$DATA_DIR" --logs-dir "$DATA_DIR")
[ ${#RELAY_ARGS[@]} -gt 0 ] && ARGS+=("${RELAY_ARGS[@]}")
[ -n "$SCENARIO" ] && ARGS+=("$SCENARIO")

if [ -z "$SCENARIO" ]; then
    echo "Running all benchmarks..."
else
    echo "Running benchmark: $SCENARIO${LATENCY_MS:+ at ${LATENCY_MS}ms latency}"
fi

RUST_LOG=warn,benchmark_test=info,whitenoise=info \
    "${CARGO_BIN[@]}" "${ARGS[@]}"

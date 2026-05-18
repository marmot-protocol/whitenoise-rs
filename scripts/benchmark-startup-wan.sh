#!/usr/bin/env bash
#
# benchmark-startup-wan.sh — Measure Whitenoise::new() warm-init under a
# simulated WAN latency.
#
# Mirrors `benchmark-startup-seeded.sh` but routes the benchmark binary's
# relay traffic through toxiproxy (docker-compose `latency` profile), with a
# configurable per-direction latency toxic on both proxies. Lets you measure
# `setup_all_subscriptions` at realistic mobile-network RTTs without depending
# on flaky public relays.
#
# Requirements:
#   - nak (https://github.com/fiatjaf/nak)
#   - Docker Compose with the project's stack already running (`just docker-up`)
#   - `python3` (for JSON pretty-printing of error responses)
#
# Usage:
#   ./scripts/benchmark-startup-wan.sh                                                # 100 ms, 5 iters
#   ./scripts/benchmark-startup-wan.sh 200                                            # 200 ms, 5 iters
#   ./scripts/benchmark-startup-wan.sh 50 10                                          # 50 ms, 10 iters
#   ./scripts/benchmark-startup-wan.sh 100 5 ./test_fixtures/nostr/other.json         # custom fixture
#
# Validated by experiments/04-toxiproxy-validation. The first iteration may be
# an outlier under high latency (TCP retry interaction); the default N=5 is
# chosen to median past it.

set -euo pipefail

source "$(dirname "$0")/toxiproxy-helpers.sh"

LATENCY_MS="${1:-100}"
ITERATIONS="${2:-5}"
FIXTURE="${3:-./test_fixtures/nostr/jeff_contacts.json}"
DATA_DIR="./dev/data/startup_bench_wan"
CONTACT_LIST_RELAY="ws://localhost:8080"
RUST_LOG_TIMING="warn,whitenoise::init_timing=info"
RUST_LOG_LOGIN="warn,whitenoise=info"

if ! [[ "$LATENCY_MS" =~ ^[0-9]+$ ]]; then
    echo "ERROR: latency_ms must be a non-negative integer (got: '$LATENCY_MS')" >&2
    exit 1
fi
if ! [[ "$ITERATIONS" =~ ^[1-9][0-9]*$ ]]; then
    echo "ERROR: iterations must be a positive integer (got: '$ITERATIONS')" >&2
    exit 1
fi

CARGO_BIN=(cargo run --bin benchmark_test --features benchmark-tests --release --)

trap toxiproxy_down EXIT

echo "=== Bringing up toxiproxy (latency profile) ==="
toxiproxy_up
toxiproxy_apply_latency "$LATENCY_MS"
echo "Latency toxic applied: ${LATENCY_MS}ms per direction (both proxies)"
echo ""

# ---------------------------------------------------------------------------
# Seed + measure (mirrors benchmark-startup-seeded.sh)
# ---------------------------------------------------------------------------

SEC=$(nak key generate)
PUB=$(echo "$SEC" | nak key public)
echo "Generated keypair: $PUB"
echo ""

echo "=== Publishing contact list to ${CONTACT_LIST_RELAY} (direct, not proxied) ==="
cat "$FIXTURE" | nak event --sec "$SEC" "$CONTACT_LIST_RELAY" 2>&1
echo ""

# Each phase that opens an encrypted database wipes "$DATA_DIR" first; the
# benchmark binary's in-memory mock keyring dies with the process, so reusing
# the directory across phases would fail to open the encrypted SQLite files.

echo "=== Cold Init (empty database) ==="
rm -rf "$DATA_DIR"
RUST_LOG="$RUST_LOG_TIMING" \
"${CARGO_BIN[@]}" --data-dir "$DATA_DIR" --logs-dir "$DATA_DIR" --init-only \
    --relays "${TOXIPROXY_RELAYS[0]}" --relays "${TOXIPROXY_RELAYS[1]}"

echo ""
echo "=== Login (syncing contacts from relays) ==="
rm -rf "$DATA_DIR"
RUST_LOG="$RUST_LOG_LOGIN" \
"${CARGO_BIN[@]}" --data-dir "$DATA_DIR" --logs-dir "$DATA_DIR" --login "$SEC" \
    --relays "${TOXIPROXY_RELAYS[0]}" --relays "${TOXIPROXY_RELAYS[1]}"

echo ""
echo "=== Warm Init (account + $ITERATIONS runs @ ${LATENCY_MS}ms latency) ==="
for i in $(seq 1 "$ITERATIONS"); do
    [ "$ITERATIONS" -gt 1 ] && echo "--- Run $i/$ITERATIONS ---"
    RUST_LOG="$RUST_LOG_TIMING" \
    "${CARGO_BIN[@]}" --data-dir "$DATA_DIR" --logs-dir "$DATA_DIR" --seed-nsec "$SEC" --init-only \
        --relays "${TOXIPROXY_RELAYS[0]}" --relays "${TOXIPROXY_RELAYS[1]}"
done

rm -rf "$DATA_DIR"

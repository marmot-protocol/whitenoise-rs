#!/usr/bin/env bash
#
# benchmark-startup-seeded.sh — Measure initialize_whitenoise() with a real account.
#
# Generates a keypair, publishes a contact list to discovery relays,
# then measures cold init (empty database) and warm init N times
# (database has account + follows).
#
# Requirements: nak (https://github.com/fiatjaf/nak)
#
# Usage:
#   ./scripts/benchmark-startup-seeded.sh                                            # 5 warm runs
#   ./scripts/benchmark-startup-seeded.sh 10                                         # 10 warm runs
#   ./scripts/benchmark-startup-seeded.sh 5 ./test_fixtures/nostr/other.json         # custom fixture

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

ITERATIONS="${1:-5}"
FIXTURE="${2:-./test_fixtures/nostr/jeff_contacts.json}"
DATA_DIR="./dev/data/startup_bench"
RELAYS="wss://nos.lol wss://relay.primal.net wss://relay.damus.io"
RUST_LOG_TIMING="warn,whitenoise::init_timing=info"
RUST_LOG_LOGIN="warn,whitenoise=info"

CARGO_BIN=(cargo run --bin benchmark_test --features benchmark-tests --release --)

# ---------------------------------------------------------------------------
# Setup: generate keypair and publish contact list
# ---------------------------------------------------------------------------

SEC=$(nak key generate)
PUB=$(echo "$SEC" | nak key public)
echo "Generated keypair: $PUB"
echo ""

echo "=== Publishing contact list to discovery relays ==="
cat "$FIXTURE" | nak event --sec "$SEC" $RELAYS 2>&1
echo ""

# ---------------------------------------------------------------------------
# Benchmark phases
# ---------------------------------------------------------------------------

rm -rf "$DATA_DIR"

echo "=== Cold Init (empty database) ==="
RUST_LOG="$RUST_LOG_TIMING" \
"${CARGO_BIN[@]}" --data-dir "$DATA_DIR" --logs-dir "$DATA_DIR" --init-only

echo ""
echo "=== Login (syncing contacts from relays) ==="
RUST_LOG="$RUST_LOG_LOGIN" \
"${CARGO_BIN[@]}" --data-dir "$DATA_DIR" --logs-dir "$DATA_DIR" --login "$SEC"

echo ""
echo "=== Warm Init (account + $ITERATIONS runs) ==="
for i in $(seq 1 "$ITERATIONS"); do
    [ "$ITERATIONS" -gt 1 ] && echo "--- Run $i/$ITERATIONS ---"
    RUST_LOG="$RUST_LOG_TIMING" \
    "${CARGO_BIN[@]}" --data-dir "$DATA_DIR" --logs-dir "$DATA_DIR" --init-only
done

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------

rm -rf "$DATA_DIR"

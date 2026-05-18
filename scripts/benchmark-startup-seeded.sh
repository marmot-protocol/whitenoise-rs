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
# Publish to the local nostr-rs-relay (port 8080) only — strfry on port 7777
# defaults to maxEventSize = 65 536 bytes and rejects contact lists larger
# than ~64 KB. The benchmark binary hard-codes both local relays as discovery
# relays, so an event landing on either one is enough for the warm-init runs
# to sync follows.
RELAYS="ws://localhost:8080"
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
#
# Every phase that opens an encrypted database wipes "$DATA_DIR" first.
# The benchmark binary uses an in-memory mock keyring, so the random DB
# encryption key generated in one process dies with it — reusing the same
# data dir across processes opens encrypted SQLite files with an empty
# keyring and fails. Each phase therefore starts from a clean slate it
# owns the keys for.
# ---------------------------------------------------------------------------

echo "=== Cold Init (empty database) ==="
rm -rf "$DATA_DIR"
RUST_LOG="$RUST_LOG_TIMING" \
"${CARGO_BIN[@]}" --data-dir "$DATA_DIR" --logs-dir "$DATA_DIR" --init-only

echo ""
echo "=== Login (syncing contacts from relays) ==="
rm -rf "$DATA_DIR"
RUST_LOG="$RUST_LOG_LOGIN" \
"${CARGO_BIN[@]}" --data-dir "$DATA_DIR" --logs-dir "$DATA_DIR" --login "$SEC"

echo ""
echo "=== Warm Init (account + $ITERATIONS runs) ==="
for i in $(seq 1 "$ITERATIONS"); do
    [ "$ITERATIONS" -gt 1 ] && echo "--- Run $i/$ITERATIONS ---"
    # --seed-nsec restores the MDK DB encryption key (saved to benchmark_keyring.txt
    # by the --login run) into the fresh in-memory mock keyring before
    # initialize_whitenoise opens the encrypted MLS database. Without this the
    # warm-init process starts with an empty keyring, finds the encrypted SQLite
    # file on disk, and fails with KeyringEntryMissingForExistingDatabase.
    RUST_LOG="$RUST_LOG_TIMING" \
    "${CARGO_BIN[@]}" --data-dir "$DATA_DIR" --logs-dir "$DATA_DIR" --seed-nsec "$SEC" --init-only
done

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------

rm -rf "$DATA_DIR"

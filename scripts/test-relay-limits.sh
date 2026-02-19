#!/usr/bin/env bash
#
# test-relay-limits.sh — Measure relay behavior for user search tuning.
#
# Tests EOSE latency vs batch size and concurrent subscription limits
# using real pubkeys from the benchmark social graph.
#
# Requirements: nak, jq, python3, timeout (coreutils)
#
# Usage:
#   ./scripts/test-relay-limits.sh              # Run all tests
#   ./scripts/test-relay-limits.sh --batch-only  # Only batch size tests
#   ./scripts/test-relay-limits.sh --concurrency-only  # Only concurrency tests

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

SEARCHER_NPUB_HEX="9236f9ac521be2ee0a54f1cfffdf2df7f4982df4e6eb992867d733debcf95b35"
SEED_RELAY="wss://relay.damus.io"

RELAYS=(
    "wss://relay.damus.io"
    "wss://relay.primal.net"
    "wss://nos.lol"
    "wss://wot.nostr.party"
    "wss://relay.satlantis.io"
    "wss://nostr-pub.wellorder.net"
    "wss://relay.wellorder.net"
    "wss://relay.snort.social"
    "wss://purplepag.es"
)

BATCH_SIZES=(40 100 200 500 750 1000)
KINDS=(0 3)  # kind 0 = metadata, kind 3 = follow list
CONCURRENCY_LEVELS=(5 10 15 20)
FETCH_TIMEOUT=15  # seconds per request
RESULTS_DIR="dev/data/relay-tests"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

log() { echo "[$(date +%H:%M:%S)] $*"; }
log_header() { echo ""; echo "═══════════════════════════════════════════════════════════════"; echo "  $*"; echo "═══════════════════════════════════════════════════════════════"; }

# Time a command and return elapsed seconds (decimal).
# Captures wall-clock time via date arithmetic.
time_cmd() {
    local start end elapsed
    start=$(python3 -c 'import time; print(time.time())')
    "$@"
    local exit_code=$?
    end=$(python3 -c 'import time; print(time.time())')
    elapsed=$(python3 -c "print(f'{$end - $start:.3f}')")
    echo "$elapsed"
    return $exit_code
}

# Build -a flags for nak from a list of pubkeys (one per line).
# Usage: build_author_flags <<< "$pubkeys"
build_author_flags() {
    local flags=()
    while IFS= read -r pk; do
        [[ -n "$pk" ]] && flags+=("-a" "$pk")
    done
    echo "${flags[@]}"
}

# ---------------------------------------------------------------------------
# Step 1: Extract pubkeys from social graph
# ---------------------------------------------------------------------------

extract_pubkeys() {
    local pubkeys_file="$RESULTS_DIR/pubkeys.txt"

    if [[ -f "$pubkeys_file" ]] && [[ $(wc -l < "$pubkeys_file") -gt 100 ]]; then
        log "Using cached pubkeys from $pubkeys_file ($(wc -l < "$pubkeys_file") keys)"
        return
    fi

    log "Fetching social graph for $SEARCHER_NPUB_HEX..."
    mkdir -p "$RESULTS_DIR"

    # Fetch the searcher's contact list (Kind 3) to get radius 1
    local contact_list
    contact_list=$(timeout 30 nak req -k 3 -a "$SEARCHER_NPUB_HEX" -l 1 "$SEED_RELAY" 2>/dev/null || true)

    if [[ -z "$contact_list" ]]; then
        echo "ERROR: Failed to fetch contact list. Check network/relay." >&2
        exit 1
    fi

    # Extract unique followed pubkeys
    local follows
    follows=$(echo "$contact_list" | jq -r '[.tags[] | select(.[0]=="p") | .[1]] | unique | .[]')
    local follow_count
    follow_count=$(echo "$follows" | wc -l | tr -d ' ')
    log "Found $follow_count follows (radius 1)"

    # For radius 2, fetch contact lists for a sample of follows.
    # We don't need ALL radius 2 — just enough pubkeys for testing (500+).
    log "Fetching radius 2 follows (sample)..."

    # Take first 20 follows and fetch their contact lists
    local r2_pubkeys=""
    local sample_follows
    sample_follows=$(echo "$follows" | head -20)

    local author_flags
    author_flags=$(echo "$sample_follows" | build_author_flags)

    local r2_contacts
    # shellcheck disable=SC2086
    r2_contacts=$(timeout 30 nak req -k 3 $author_flags "$SEED_RELAY" 2>/dev/null || true)

    if [[ -n "$r2_contacts" ]]; then
        r2_pubkeys=$(echo "$r2_contacts" | jq -r '[.tags[] | select(.[0]=="p") | .[1]] | .[]')
    fi

    # Combine all unique pubkeys and convert to npub (nak rejects some valid hex)
    local hex_file="$RESULTS_DIR/pubkeys-hex.txt"
    {
        echo "$follows"
        echo "$r2_pubkeys"
    } | sort -u | grep -v '^$' > "$hex_file"

    log "Converting $(wc -l < "$hex_file" | tr -d ' ') hex pubkeys to npub format..."
    : > "$pubkeys_file"
    while IFS= read -r pk; do
        local npub
        npub=$(timeout 2 nak encode npub "$pk" 2>/dev/null)
        [[ -n "$npub" ]] && echo "$npub" >> "$pubkeys_file"
    done < "$hex_file"

    local total
    total=$(wc -l < "$pubkeys_file" | tr -d ' ')
    log "Total unique pubkeys: $total (saved to $pubkeys_file)"
}

# ---------------------------------------------------------------------------
# Test A: EOSE timing vs batch size
# ---------------------------------------------------------------------------

test_batch_sizes() {
    local pubkeys_file="$RESULTS_DIR/pubkeys.txt"
    local results_file="$RESULTS_DIR/batch-results.csv"

    log_header "TEST A: EOSE Timing vs Batch Size"
    echo "relay,kind,batch_size,events_returned,elapsed_secs,status" > "$results_file"

    # Load pubkeys
    local total
    total=$(wc -l < "$pubkeys_file" | tr -d ' ')
    log "Using $total pubkeys for batch tests"
    log "Kinds: ${KINDS[*]}"

    for relay in "${RELAYS[@]}"; do
        log ""
        log "--- $relay ---"

        for kind in "${KINDS[@]}"; do
            for batch_size in "${BATCH_SIZES[@]}"; do
                # Take first N pubkeys for this batch
                local actual_size=$((batch_size > total ? total : batch_size))

                # Build author flags from first N lines
                local flags=()
                while IFS= read -r pk; do
                    flags+=("-a" "$pk")
                done < <(head -n "$actual_size" "$pubkeys_file")

                # Run the request and time it
                local tmpfile
                tmpfile=$(mktemp)
                local start end elapsed event_count status

                start=$(python3 -c 'import time; print(time.time())')

                if timeout "$FETCH_TIMEOUT" nak req -k "$kind" "${flags[@]}" "$relay" > "$tmpfile" 2>/dev/null; then
                    status="ok"
                else
                    local exit_code=$?
                    if [[ $exit_code -eq 124 ]]; then
                        status="timeout"
                    else
                        status="error"
                    fi
                fi

                end=$(python3 -c 'import time; print(time.time())')
                elapsed=$(python3 -c "print(f'{$end - $start:.3f}')")
                event_count=$(wc -l < "$tmpfile" | tr -d ' ')
                rm -f "$tmpfile"

                log "  kind=$kind  batch=$actual_size  events=$event_count  time=${elapsed}s  status=$status"
                echo "$relay,$kind,$actual_size,$event_count,$elapsed,$status" >> "$results_file"
            done
        done
    done

    log ""
    log "Batch results saved to $results_file"
}

# ---------------------------------------------------------------------------
# Test B: Concurrent subscriptions
# ---------------------------------------------------------------------------

test_concurrency() {
    local pubkeys_file="$RESULTS_DIR/pubkeys.txt"
    local results_file="$RESULTS_DIR/concurrency-results.csv"

    log_header "TEST B: Concurrent Subscription Limits"
    echo "relay,kind,concurrency,successful,failed,timed_out,total_elapsed_secs" > "$results_file"

    # We'll use small batches (10 pubkeys each) to isolate
    # concurrency effects from batch size effects

    for relay in "${RELAYS[@]}"; do
        log ""
        log "--- $relay ---"

        for kind in "${KINDS[@]}"; do
            for concurrency in "${CONCURRENCY_LEVELS[@]}"; do
                # Create N independent requests, each with a different set of 10 pubkeys
                local pids=()
                local tmpdir
                tmpdir=$(mktemp -d)

                local start
                start=$(python3 -c 'import time; print(time.time())')

                for ((i = 0; i < concurrency; i++)); do
                    local offset=$((i * 10 + 1))  # 1-indexed for sed
                    local flags=()
                    while IFS= read -r pk; do
                        flags+=("-a" "$pk")
                    done < <(sed -n "${offset},$((offset + 9))p" "$pubkeys_file")

                    (
                        if timeout "$FETCH_TIMEOUT" nak req -k "$kind" "${flags[@]}" "$relay" > "$tmpdir/result_$i" 2>/dev/null; then
                            echo "ok" > "$tmpdir/status_$i"
                        else
                            local ec=$?
                            if [[ $ec -eq 124 ]]; then
                                echo "timeout" > "$tmpdir/status_$i"
                            else
                                echo "error" > "$tmpdir/status_$i"
                            fi
                        fi
                    ) &
                    pids+=($!)
                done

                # Wait for all to complete
                for pid in "${pids[@]}"; do
                    wait "$pid" 2>/dev/null || true
                done

                local end
                end=$(python3 -c 'import time; print(time.time())')
                local elapsed
                elapsed=$(python3 -c "print(f'{$end - $start:.3f}')")

                # Count results
                local ok=0 fail=0 tout=0
                for ((i = 0; i < concurrency; i++)); do
                    local s
                    s=$(cat "$tmpdir/status_$i" 2>/dev/null || echo "error")
                    case "$s" in
                        ok) ((ok++)) || true ;;
                        timeout) ((tout++)) || true ;;
                        *) ((fail++)) || true ;;
                    esac
                done

                rm -rf "$tmpdir"
                log "  kind=$kind  concurrency=$concurrency  ok=$ok  failed=$fail  timeout=$tout  time=${elapsed}s"
                echo "$relay,$kind,$concurrency,$ok,$fail,$tout,$elapsed" >> "$results_file"
            done
        done
    done

    log ""
    log "Concurrency results saved to $results_file"
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

print_summary() {
    log_header "SUMMARY"

    if [[ -f "$RESULTS_DIR/batch-results.csv" ]]; then
        log ""
        log "=== Batch Size Results ==="
        log ""
        printf "%-35s %5s %6s %8s %10s %8s\n" "Relay" "Kind" "Batch" "Events" "Time (s)" "Status"
        printf "%-35s %5s %6s %8s %10s %8s\n" "---" "---" "---" "---" "---" "---"
        tail -n +2 "$RESULTS_DIR/batch-results.csv" | while IFS=, read -r relay kind batch events elapsed status; do
            printf "%-35s %5s %6s %8s %10s %8s\n" "$relay" "$kind" "$batch" "$events" "$elapsed" "$status"
        done
    fi

    if [[ -f "$RESULTS_DIR/concurrency-results.csv" ]]; then
        log ""
        log "=== Concurrency Results ==="
        log ""
        printf "%-35s %5s %6s %4s %6s %8s %10s\n" "Relay" "Kind" "Conc." "OK" "Fail" "Timeout" "Time (s)"
        printf "%-35s %5s %6s %4s %6s %8s %10s\n" "---" "---" "---" "---" "---" "---" "---"
        tail -n +2 "$RESULTS_DIR/concurrency-results.csv" | while IFS=, read -r relay kind conc ok fail tout elapsed; do
            printf "%-35s %5s %6s %4s %6s %8s %10s\n" "$relay" "$kind" "$conc" "$ok" "$fail" "$tout" "$elapsed"
        done
    fi
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

main() {
    local mode="${1:-all}"

    log "Relay Behavior Test Suite"
    log "========================"
    log "Relays: ${#RELAYS[@]}"
    log "Batch sizes: ${BATCH_SIZES[*]}"
    log "Concurrency levels: ${CONCURRENCY_LEVELS[*]}"
    log "Timeout per request: ${FETCH_TIMEOUT}s"

    mkdir -p "$RESULTS_DIR"
    extract_pubkeys

    case "$mode" in
        --batch-only)
            test_batch_sizes
            ;;
        --concurrency-only)
            test_concurrency
            ;;
        all|*)
            test_batch_sizes
            test_concurrency
            ;;
    esac

    print_summary
    log ""
    log "Raw data saved to $RESULTS_DIR/"
}

main "$@"

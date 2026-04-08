######################
# Development
######################

# Default recipe - show available commands
default:
    @just --list

# Clear local data dirs (ensure you're not running docker compose before running)
clear-dev-data:
    rm -rf ./dev/data/*

# Run CLI end-to-end tests (requires local relays via docker compose)
# Usage:
#   just e2e-test                                      # Run all CLI E2E tests
#   just e2e-test account_profile_and_export           # Run a test in cli_e2e
#   just e2e-test relay_control_snapshot_tracks_live_planes  # Run a test in cli_relay_control_e2e
e2e-test test="":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "{{test}}" ]; then
        echo "Running all CLI E2E tests..."
        cargo test --features cli,integration-tests --test cli_e2e --test cli_relay_control_e2e
    else
        echo "Running CLI E2E test: {{test}}"
        matched=0
        if cargo test --features cli,integration-tests --test cli_e2e -- --list | grep -Eq "^{{test}}:"; then
            matched=1
            cargo test --features cli,integration-tests --test cli_e2e "{{test}}"
        fi
        if cargo test --features cli,integration-tests --test cli_relay_control_e2e -- --list | grep -Eq "^{{test}}:"; then
            matched=1
            cargo test --features cli,integration-tests --test cli_relay_control_e2e "{{test}}"
        fi
        if [ "$matched" -eq 0 ]; then
            echo "No CLI E2E test named '{{test}}' found in cli_e2e or cli_relay_control_e2e" >&2
            exit 1
        fi
    fi

# Run integration_test binary using the local relays and data dirs
# Usage:
#   just int-test                      # Run all integration tests
#   just int-test basic-messaging      # Run specific scenario
int-test scenario="":
    #!/usr/bin/env bash
    set -euo pipefail
    rm -rf ./dev/data/integration_test/
    if [ -z "{{scenario}}" ]; then
        echo "Running all integration test scenarios..."
        RUST_LOG=debug,\
        sqlx=info,\
        refinery_core=error,\
        keyring=info,\
        nostr_relay_pool=error,\
        mdk_sqlite_storage=error,\
        tungstenite=error,\
        integration_test=debug \
        cargo run --bin integration_test --features integration-tests -- \
        --data-dir ./dev/data/integration_test/ --logs-dir ./dev/data/integration_test/
    else
        echo "Running integration test scenario: {{scenario}}"
        RUST_LOG=debug,\
        sqlx=info,\
        refinery_core=error,\
        keyring=info,\
        nostr_relay_pool=error,\
        mdk_sqlite_storage=error,\
        tungstenite=error,\
        integration_test=debug \
        cargo run --bin integration_test --features integration-tests -- \
        --data-dir ./dev/data/integration_test/ --logs-dir ./dev/data/integration_test/ \
        "{{scenario}}"
    fi

# Run integration_test binary with flamegraph to analyze performance
int-test-flamegraph:
    rm -rf ./dev/data/integration_test/ cargo-flamegraph.trace flamegraph.svg
    RUST_LOG=debug,\
    sqlx=info,\
    refinery_core=error,\
    keyring=info,\
    nostr_relay_pool=error,\
    mdk_sqlite_storage=error,\
    tungstenite=error,\
    integration_test=debug \
    CARGO_PROFILE_RELEASE_DEBUG=true \
    CARGO_PROFILE_RELEASE_LTO=false \
    CARGO_PROFILE_RELEASE_STRIP=false \
    cargo flamegraph --release --bin integration_test --features integration-tests --deterministic -- --data-dir ./dev/data/integration_test/ --logs-dir ./dev/data/integration_test/

# Run performance benchmarks (not included in CI)
# Usage:
#   just benchmark                      # Run all benchmarks
#   just benchmark messaging-performance  # Run specific benchmark
benchmark scenario="":
    #!/usr/bin/env bash
    set -euo pipefail
    rm -rf ./dev/data/benchmark_test/
    if [ -z "{{scenario}}" ]; then
        echo "Running all benchmarks..."
        RUST_LOG=warn,benchmark_test=info,whitenoise=info \
        cargo run --bin benchmark_test --features benchmark-tests --release -- \
        --data-dir ./dev/data/benchmark_test/ --logs-dir ./dev/data/benchmark_test/
    else
        echo "Running benchmark: {{scenario}}"
        RUST_LOG=warn,benchmark_test=info,whitenoise=info \
        cargo run --bin benchmark_test --features benchmark-tests --release -- \
        --data-dir ./dev/data/benchmark_test/ --logs-dir ./dev/data/benchmark_test/ \
        "{{scenario}}"
    fi
    rm -rf ./dev/data/benchmark_test/

# Run benchmarks and emit JSON output
# Usage:
#   just benchmark-json                      # All scenarios
#   just benchmark-json messaging-performance  # Specific scenario
benchmark-json scenario="":
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p ./benchmark_results
    rm -rf ./dev/data/benchmark_test/ && mkdir -p ./dev/data/benchmark_test
    TIMESTAMP=$(date +%Y%m%d_%H%M%S)
    SUFFIX=""
    [ -n "{{scenario}}" ] && SUFFIX="_{{scenario}}"
    RUST_LOG=warn,benchmark_test=info,whitenoise=info \
        cargo run --release --features benchmark-tests --bin benchmark_test -- \
        --data-dir ./dev/data/benchmark_test \
        --logs-dir ./dev/data/benchmark_test/logs \
        --output-json "./benchmark_results/result_${TIMESTAMP}${SUFFIX}.json" \
        {{scenario}}
    rm -rf ./dev/data/benchmark_test

# Run only stable-tier benchmark scenarios and merge results into a single JSON.
# Used by the merge-gating CI job so relay-tier failures cannot block a merge.
benchmark-json-stable:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v jq >/dev/null 2>&1 || { echo "ERROR: jq is required but not installed"; exit 1; }
    mkdir -p ./benchmark_results
    rm -rf ./dev/data/benchmark_test/ && mkdir -p ./dev/data/benchmark_test
    TIMESTAMP=$(date +%Y%m%d_%H%M%S)
    MERGED="./benchmark_results/result_${TIMESTAMP}_stable.json"
    TMPDIR=$(mktemp -d)
    trap 'rm -rf "$TMPDIR"' EXIT
    # Run each stable scenario individually so a single-scenario failure is isolated.
    SCENARIO_FILES=()
    for SCENARIO in messaging-performance message-aggregation identity-creation; do
        RUST_LOG=warn,benchmark_test=info,whitenoise=info \
            cargo run --release --features benchmark-tests --bin benchmark_test -- \
            --data-dir ./dev/data/benchmark_test \
            --logs-dir ./dev/data/benchmark_test/logs \
            --output-json "$TMPDIR/${SCENARIO}.json" \
            "$SCENARIO"
        SCENARIO_FILES+=("$TMPDIR/${SCENARIO}.json")
    done
    # Merge the single-scenario JSON files into one envelope.
    # jq slurps all files, takes generated_at/git_sha from the last file,
    # and concatenates all scenarios[] arrays.
    jq -s '{
        generated_at: .[-1].generated_at,
        git_sha: .[-1].git_sha,
        scenarios: [.[].scenarios[]]
    }' "${SCENARIO_FILES[@]}" > "$MERGED"
    rm -rf ./dev/data/benchmark_test
    echo "Stable benchmark results: $MERGED"

# Set current result as regression baseline
# Usage:
#   just benchmark-baseline                      # All scenarios
#   just benchmark-baseline messaging-performance  # Specific scenario
benchmark-baseline scenario="":
    #!/usr/bin/env bash
    set -euo pipefail
    just benchmark-json {{scenario}}
    shopt -s nullglob
    files=(./benchmark_results/result_*.json)
    shopt -u nullglob
    if [ ${#files[@]} -eq 0 ]; then
        echo "ERROR: No benchmark result file found"
        exit 1
    fi
    RESULT=$(printf '%s\0' "${files[@]}" | xargs -0 ls -t | head -1)
    cp "$RESULT" ./benchmark_results/baseline.json
    echo "Baseline updated: ./benchmark_results/baseline.json"

# Run benchmarks and compare against baseline
# Usage:
#   just benchmark-check                      # All scenarios
#   just benchmark-check messaging-performance  # Specific scenario
benchmark-check scenario="":
    #!/usr/bin/env bash
    set -euo pipefail
    just benchmark-json {{scenario}}
    shopt -s nullglob
    files=(./benchmark_results/result_*.json)
    shopt -u nullglob
    if [ ${#files[@]} -eq 0 ]; then
        echo "ERROR: No benchmark result file found"
        exit 1
    fi
    CANDIDATE=$(printf '%s\0' "${files[@]}" | xargs -0 ls -t | head -1)
    cargo run --release --features benchmark-tests --bin bench_compare -- \
        ./benchmark_results/baseline.json \
        "$CANDIDATE" \
        {{ if scenario != "" { "--scenario " + scenario } else { "" } }}

# Run benchmarks with per-iteration detail and emit a Perfetto trace
# Usage:
#   just benchmark-trace                      # All scenarios
#   just benchmark-trace messaging-performance  # Specific scenario
benchmark-trace scenario="":
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p ./benchmark_results
    TS=$(date +%Y%m%d_%H%M%S)
    SUFFIX=""
    [ -n "{{scenario}}" ] && SUFFIX="_{{scenario}}"
    rm -rf ./dev/data/benchmark_test && mkdir -p ./dev/data/benchmark_test
    RUST_LOG=warn,benchmark_test=info,whitenoise=info \
        cargo run --release --features benchmark-tests --bin benchmark_test -- \
        --data-dir ./dev/data/benchmark_test \
        --logs-dir ./dev/data/benchmark_test/logs \
        --output-json "./benchmark_results/trace_${TS}${SUFFIX}.json" \
        --chrome-trace "./benchmark_results/trace_${TS}${SUFFIX}.perfetto.json" \
        --detailed \
        {{scenario}}
    rm -rf ./dev/data/benchmark_test
    echo "Perfetto trace: ./benchmark_results/trace_${TS}${SUFFIX}.perfetto.json"
    echo "Open at: https://ui.perfetto.dev"


# Measure initialization timing (phase-by-phase breakdown, empty database)
# Usage:
#   just benchmark-startup       # Single run
#   just benchmark-startup 5     # 5 runs
benchmark-startup iterations="1":
    #!/usr/bin/env bash
    set -euo pipefail
    for i in $(seq 1 {{iterations}}); do
        [ {{iterations}} -gt 1 ] && echo "--- Run $i/{{iterations}} ---"
        rm -rf ./dev/data/startup_bench/
        RUST_LOG=warn,whitenoise::init_timing=info \
        cargo run --bin benchmark_test --features benchmark-tests --release -- \
        --data-dir ./dev/data/startup_bench/ --logs-dir ./dev/data/startup_bench/ \
        --init-only
    done
    rm -rf ./dev/data/startup_bench/

# Measure initialization timing with a real account and follow list.
# Requires: nak (https://github.com/fiatjaf/nak)
# Usage:
#   just benchmark-startup-seeded                                         # 5 warm runs
#   just benchmark-startup-seeded 10                                      # 10 warm runs
#   just benchmark-startup-seeded 5 ./test_fixtures/nostr/other.json      # custom fixture
benchmark-startup-seeded iterations="5" fixture="./test_fixtures/nostr/jeff_contacts.json":
    @bash scripts/benchmark-startup-seeded.sh {{iterations}} {{fixture}}

# Run benchmarks and save results with timestamp
# Usage:
#   just benchmark-save                      # Save all benchmarks
#   just benchmark-save messaging-performance  # Save specific benchmark
benchmark-save scenario="":
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p ./benchmark_results
    rm -rf ./dev/data/benchmark_test/
    TIMESTAMP=$(date +%Y%m%d_%H%M%S)

    if [ -z "{{scenario}}" ]; then
        FILENAME="benchmark_${TIMESTAMP}.log"
        echo "Running all benchmarks and saving to ./benchmark_results/${FILENAME}"
        RUST_LOG=warn,benchmark_test=info,whitenoise=info \
        cargo run --bin benchmark_test --features benchmark-tests --release -- \
        --data-dir ./dev/data/benchmark_test/ \
        --logs-dir ./dev/data/benchmark_test/ \
        | tee ./benchmark_results/${FILENAME}
    else
        FILENAME="benchmark_{{scenario}}_${TIMESTAMP}.log"
        echo "Running benchmark '{{scenario}}' and saving to ./benchmark_results/${FILENAME}"
        RUST_LOG=warn,benchmark_test=info,whitenoise=info \
        cargo run --bin benchmark_test --features benchmark-tests --release -- \
        --data-dir ./dev/data/benchmark_test/ \
        --logs-dir ./dev/data/benchmark_test/ \
        "{{scenario}}" \
        | tee ./benchmark_results/${FILENAME}
    fi

    rm -rf ./dev/data/benchmark_test/
    echo "Results saved to ./benchmark_results/${FILENAME}"

# Run all tests (unit tests, integration tests, and doc tests)
# CLI e2e binaries are excluded here — run them explicitly with `just e2e-test`.
# Uses nextest for faster parallel execution if available, falls back to cargo test
test:
    #!/usr/bin/env bash
    set -euo pipefail
    if command -v cargo-nextest &> /dev/null; then
        echo "Running tests with nextest (parallel)..."
        cargo nextest run --all-features --all-targets \
            --filter-expr 'not binary(cli_e2e) and not binary(cli_relay_control_e2e)'
        cargo test --all-features --doc
    else
        echo "Running tests with cargo test..."
        echo "Tip: Install cargo-nextest for faster parallel testing: just install-tools"
        cargo test --all-features --all-targets
        cargo test --all-features --doc
    fi

# Run tests with standard cargo test (slower, sequential)
test-cargo:
    cargo test --all-features --all-targets
    cargo test --all-features --doc

# Check clippy
check-clippy:
    @bash scripts/check-clippy.sh

# Fix clippy issues automatically where possible
fix-clippy:
    cargo clippy --all-targets --all-features --fix --allow-dirty --allow-staged

# Check fmt
check-fmt:
    @bash scripts/check-fmt.sh check

# Apply formatting
fmt:
    @bash scripts/check-fmt.sh

# Check docs
check-docs:
    @bash scripts/check-docs.sh

# Check for #[allow(dead_code)] annotations
check-dead-code-allows:
    @bash scripts/check-dead-code-allows.sh

# Check all (fast checks before running tests)
check:
    @bash scripts/check-all.sh

# Pre-commit checks: quiet mode with minimal output (recommended for agents/CI)
precommit:
    @just _run-quiet "check-fmt"                "fmt"
    @just _run-quiet "check-docs"               "docs"
    @just _run-quiet "check-clippy"             "clippy"
    @just _run-quiet "check-dead-code-allows"   "dead_code"
    @just _run-quiet "test"                     "tests"
    @just _run-quiet "int-test"                 "integration tests"
    @echo "PRECOMMIT PASSED"

# Pre-commit checks with verbose output (shows all command output)
precommit-verbose: check test int-test

# Quick pre-commit: quiet mode, skip integration tests (recommended for agents/CI)
precommit-quick:
    @just _run-quiet "check-fmt"                "fmt"
    @just _run-quiet "check-docs"               "docs"
    @just _run-quiet "check-clippy"             "clippy"
    @just _run-quiet "check-dead-code-allows"   "dead_code"
    @just _run-quiet "test-unit"                "tests"
    @echo "PRECOMMIT PASSED"

# Unit tests only (no integration-tests feature, no e2e binaries).
# Used by precommit-quick for fast local validation without Docker.
test-unit:
    #!/usr/bin/env bash
    set -euo pipefail
    if command -v cargo-nextest &> /dev/null; then
        echo "Running unit tests with nextest (parallel)..."
        cargo nextest run --features cli --all-targets \
            --filter-expr 'not binary(cli_e2e) and not binary(cli_relay_control_e2e)'
        cargo test --features cli --doc
    else
        echo "Running unit tests with cargo test..."
        cargo test --features cli --all-targets
        cargo test --features cli --doc
    fi

# Quick pre-commit with verbose output
precommit-quick-verbose: check test

# Check for outdated dependencies
outdated:
    cargo outdated --workspace --root-deps-only

# Update dependencies
update:
    cargo update

# Audit dependencies for security vulnerabilities
# Ignoring:
# - RUSTSEC-2023-0071: RSA Marvin Attack (transitive via sqlx-mysql, not used in our SQLite-only app)
# - RUSTSEC-2024-0384: instant unmaintained (transitive via rust-nostr, low risk)
# - RUSTSEC-2026-0002: lru unsound (transitive via nostr-sdk, awaiting upstream fix)
# - RUSTSEC-2026-0037: quinn-proto DoS via malformed QUIC handshake (transitive via nostr-blossom → reqwest → quinn; awaiting upstream fix)
audit:
    cargo audit --ignore RUSTSEC-2023-0071 --ignore RUSTSEC-2024-0384 --ignore RUSTSEC-2026-0002 --ignore RUSTSEC-2026-0037

# Generate and open documentation
doc:
    cargo doc --no-deps --all-features --open

# Clean build artifacts and data
clean:
    cargo clean
    rm -rf ./dev/data/*

# Force running tests with nextest (requires nextest to be installed)
test-nextest:
    cargo nextest run --all-features --all-targets
    cargo test --all-features --doc

# Filename regex for excluding test infrastructure from coverage metrics
coverage_ignore := '(integration_tests/|bin/integration_test\.rs|bin/benchmark_test\.rs)'

# Generate code coverage report (lcov format, matches CI flags)
coverage:
    cargo llvm-cov clean --workspace
    cargo llvm-cov --workspace --all-targets --features cli \
      --ignore-filename-regex '{{coverage_ignore}}' \
      --lcov --output-path lcov.info
    @echo "Coverage report: lcov.info"

# Generate HTML code coverage report (matches CI flags)
coverage-html:
    cargo llvm-cov clean --workspace
    cargo llvm-cov --workspace --all-targets --features cli \
      --ignore-filename-regex '{{coverage_ignore}}' \
      --html
    @echo "HTML report: target/llvm-cov/html/index.html"

# Check minimum supported Rust version
check-msrv:
    cargo msrv verify

# Run cargo-deny checks (licenses, security, sources)
deny-check:
    cargo deny check

# Install recommended development tools
install-tools:
    @bash scripts/install-dev-tools.sh

######################
# Build
######################

# Build release binary
build-release:
    cargo build --release

######################
# Docker
######################

# Build the local Transponder image without starting the stack
docker-build:
    ./scripts/ensure_transponder_env.sh
    docker compose build transponder

# Start docker compose services and wait for them to become ready
docker-up:
    ./scripts/ensure_transponder_env.sh
    docker compose up -d
    ./scripts/wait_for_dev_services.sh

# Wait for local docker services to become ready
docker-smoke:
    ./scripts/wait_for_dev_services.sh

# Stop docker compose services
docker-down:
    docker compose down -v

# Show docker compose logs
docker-logs:
    docker compose logs -f

######################
# Utilities
######################

# Publish a NIP-89 handler
publish-nip89:
    ./scripts/publish_nip89_handler.sh

######################
# Helper Recipes
######################

# Run a recipe quietly, showing only name and pass/fail status (internal use)
[private]
_run-quiet recipe label:
    #!/usr/bin/env bash
    TMPFILE=$(mktemp)
    trap 'rm -f "$TMPFILE"' EXIT
    printf "%-25s" "{{label}}..."
    if just {{recipe}} > "$TMPFILE" 2>&1; then
        echo "✓"
    else
        echo "✗"
        echo ""
        cat "$TMPFILE"
        exit 1
    fi

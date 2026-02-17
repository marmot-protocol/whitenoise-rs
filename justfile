######################
# Development
######################

# Default recipe - show available commands
default:
    @just --list

# Clear local data dirs (ensure you're not running docker compose before running)
clear-dev-data:
    rm -rf ./dev/data/*

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
# Uses nextest for faster parallel execution if available, falls back to cargo test
test:
    #!/usr/bin/env bash
    set -euo pipefail
    if command -v cargo-nextest &> /dev/null; then
        echo "Running tests with nextest (parallel)..."
        cargo nextest run --all-features --all-targets
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

# Check all (fast checks before running tests)
check:
    @bash scripts/check-all.sh

# Pre-commit checks: quiet mode with minimal output (recommended for agents/CI)
precommit:
    @just _run-quiet "check-fmt"    "fmt"
    @just _run-quiet "check-docs"   "docs"
    @just _run-quiet "check-clippy" "clippy"
    @just _run-quiet "test"         "tests"
    @just _run-quiet "int-test"     "integration tests"
    @echo "PRECOMMIT PASSED"

# Pre-commit checks with verbose output (shows all command output)
precommit-verbose: check test int-test

# Quick pre-commit: quiet mode, skip integration tests (recommended for agents/CI)
precommit-quick:
    @just _run-quiet "check-fmt"    "fmt"
    @just _run-quiet "check-docs"   "docs"
    @just _run-quiet "check-clippy" "clippy"
    @just _run-quiet "test"         "tests"
    @echo "PRECOMMIT PASSED"

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
audit:
    cargo audit --ignore RUSTSEC-2023-0071 --ignore RUSTSEC-2024-0384 --ignore RUSTSEC-2026-0002

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
    cargo llvm-cov --workspace --all-targets \
      --ignore-filename-regex '{{coverage_ignore}}' \
      --lcov --output-path lcov.info
    @echo "Coverage report: lcov.info"

# Generate HTML code coverage report (matches CI flags)
coverage-html:
    cargo llvm-cov clean --workspace
    cargo llvm-cov --workspace --all-targets \
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

# Start docker compose services
docker-up:
    docker compose up -d

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

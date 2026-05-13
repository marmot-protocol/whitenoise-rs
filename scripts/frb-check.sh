#!/usr/bin/env bash
set -euo pipefail

# Lint, format-check, and compile-check the FRB wrapper crate. Used by the
# codegen workflow; safe to run locally. Catches wrapper-only regressions
# (added Rust APIs that don't compile, missing trait impls, drift in the
# generated `frb_generated.rs` after a regen, etc.).

echo "🔍 Format-checking rust_lib_whitenoise..."
cargo fmt --package rust_lib_whitenoise --check

echo "🔍 Linting rust_lib_whitenoise..."
cargo clippy \
    --no-deps \
    --package rust_lib_whitenoise \
    --all-targets \
    -- -D warnings -A clippy::uninlined_format_args

echo "✅ rust_lib_whitenoise checks passed"

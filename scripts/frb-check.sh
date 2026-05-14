#!/usr/bin/env bash
set -euo pipefail

# Lint, format-check, and compile-check the FRB wrapper crate. Used by the
# codegen workflow; safe to run locally. Catches wrapper-only regressions
# (added Rust APIs that don't compile, missing trait impls, drift in the
# generated `frb_generated.rs` after a regen, etc.).

echo "🔍 Format-checking whitenoise_frb..."
cargo fmt --package whitenoise_frb --check

echo "🔍 Linting whitenoise_frb..."
cargo clippy \
    --no-deps \
    --package whitenoise_frb \
    --all-targets \
    -- -D warnings -A clippy::uninlined_format_args

echo "✅ whitenoise_frb checks passed"

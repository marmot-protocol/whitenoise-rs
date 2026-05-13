#!/usr/bin/env bash
set -euo pipefail

# Lint and compile-check the FRB wrapper crate. The repo-wide check-clippy.sh
# only inspects the root `whitenoise` crate, so this catches wrapper-only
# regressions (added Rust APIs that don't compile, missing trait impls, etc.).

echo "🔍 Checking rust_lib_whitenoise..."
cargo clippy \
    --no-deps \
    --package rust_lib_whitenoise \
    --all-targets \
    -- -D warnings -A clippy::uninlined_format_args

echo "✅ rust_lib_whitenoise checks passed"

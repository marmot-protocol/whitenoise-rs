#!/usr/bin/env bash
set -euo pipefail

# Pinned to match the version pinned in crates/whitenoise-frb/Cargo.toml so
# that the generator and the runtime bindings stay in lockstep.
FRB_VERSION="2.11.1"
STAGING=".frb-staging"

if ! command -v flutter_rust_bridge_codegen >/dev/null 2>&1; then
    echo "flutter_rust_bridge_codegen not installed. Run:"
    echo "    cargo install flutter_rust_bridge_codegen --version ${FRB_VERSION} --locked"
    exit 1
fi

INSTALLED_VERSION=$(flutter_rust_bridge_codegen --version | awk '{print $NF}')
if [[ "${INSTALLED_VERSION}" != "${FRB_VERSION}" ]]; then
    echo "flutter_rust_bridge_codegen version mismatch: installed ${INSTALLED_VERSION}, expected ${FRB_VERSION}"
    echo "Reinstall with: cargo install flutter_rust_bridge_codegen --version ${FRB_VERSION} --locked --force"
    exit 1
fi

if [[ ! -d "${STAGING}" ]]; then
    echo "✗ ${STAGING}/ missing — set it up with 'just frb-stage' first." >&2
    echo "  This directory is a worktree of the 'flutter-package' orphan branch and" >&2
    echo "  receives the codegen output (no Dart files are committed to master)." >&2
    exit 1
fi

if [[ ! -f "${STAGING}/pubspec.yaml" ]]; then
    echo "✗ ${STAGING}/pubspec.yaml missing — staging worktree is incomplete." >&2
    echo "  Re-run 'just frb-stage' to refresh the worktree from origin." >&2
    exit 1
fi

mkdir -p "${STAGING}/lib/src/rust"

echo "Regenerating FRB bindings into ${STAGING}/lib/src/rust/..."
flutter_rust_bridge_codegen generate

if command -v dart >/dev/null 2>&1; then
    echo "Formatting generated Dart files..."
    dart format --page-width 100 "${STAGING}/lib/src/rust" >/dev/null
else
    echo "warning: dart cli not found; generated bindings will not be formatted." >&2
    echo "         install Flutter or Dart and re-run for clean diffs." >&2
fi

if command -v cargo >/dev/null 2>&1; then
    echo "Formatting generated Rust file..."
    cargo fmt --package rust_lib_whitenoise 2>/dev/null || true
fi

echo "Done. Inspect changes with: git -C ${STAGING} diff"

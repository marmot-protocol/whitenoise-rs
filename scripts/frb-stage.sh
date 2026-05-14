#!/usr/bin/env bash
set -eo pipefail

# Populate .frb-staging/ as a Flutter package staging directory by copying
# the vendored template from crates/whitenoise-frb/template/. This is the
# input to `just frb-generate`, which writes generated Dart into
# .frb-staging/lib/src/rust/ and vendors the wrapper crate into
# .frb-staging/rust/.
#
# Replaces the old orphan-branch worktree approach. master now contains
# the full package template so codegen is self-contained.

STAGING=".frb-staging"
TEMPLATE="crates/whitenoise-frb/template"

if [[ ! -d "${TEMPLATE}" ]]; then
    echo "✗ Template not found at ${TEMPLATE}." >&2
    echo "  This script must run from the repo root." >&2
    exit 1
fi

# Detect a legacy orphan-branch worktree from the previous distribution
# scheme. `git worktree remove` deregisters it cleanly so the worktree
# registry doesn't end up with a dangling pointer.
if [[ -f "${STAGING}/.git" ]] && grep -q '^gitdir:' "${STAGING}/.git" 2>/dev/null; then
    echo "Detected legacy orphan-branch worktree at ${STAGING}/. Removing..."
    git worktree remove --force "${STAGING}"
fi

# At this point ${STAGING}/ may still exist as a regular dir from a prior
# run. Refuse to touch it if it contains anything unexpected so we never
# silently clobber user state.
if [[ -d "${STAGING}" ]]; then
    # Anything other than lib/, rust/, .dart_tool/, pubspec.lock, or files
    # that already match the template is unexpected.
    UNEXPECTED=$(find "${STAGING}" -maxdepth 1 -mindepth 1 \
        ! -name 'lib' ! -name 'rust' ! -name '.dart_tool' ! -name 'pubspec.lock' \
        ! -name '.' ! -name '..' \
        -print 2>/dev/null | head -1)
    if [[ -n "${UNEXPECTED}" ]]; then
        echo "  Refreshing template content (preserving lib/, rust/, .dart_tool/, pubspec.lock)..."
    fi
fi

mkdir -p "${STAGING}"

# Overlay the template. tar overwrites existing files but doesn't delete
# stale ones — that's fine here because the template is additive: lib/ and
# rust/ are managed by frb-generate.sh and never appear in the template.
(cd "${TEMPLATE}" && tar -cf - .) | tar -xf - -C "${STAGING}/"

# Pre-create the directories codegen and vendor will write to, so error
# messages from frb-generate.sh stay actionable.
mkdir -p "${STAGING}/lib/src/rust" "${STAGING}/rust"

echo "✓ ${STAGING}/ populated from ${TEMPLATE}/."
echo "  Run 'just frb-generate' to populate Dart bindings + vendor wrapper."

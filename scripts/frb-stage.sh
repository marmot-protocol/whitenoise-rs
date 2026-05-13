#!/usr/bin/env bash
set -euo pipefail

# Set up .frb-staging/ as a git worktree of the `flutter-package` orphan
# branch. This directory is gitignored on master and is the dart_output target
# for `just frb-generate`.

STAGING=".frb-staging"
BRANCH="flutter-package"

if [[ -d "${STAGING}/.git" || -f "${STAGING}/.git" ]]; then
    # Refuse to touch ${STAGING}/ unless it's actually a worktree of the
    # ${BRANCH} branch — a misplaced clone or wrong-branch worktree must not
    # be silently fast-forwarded or hard-reset.
    current_branch=$(git -C "${STAGING}" symbolic-ref --short HEAD 2>/dev/null || echo "")
    if [[ "${current_branch}" != "${BRANCH}" ]]; then
        echo "✗ ${STAGING}/ exists but is not on '${BRANCH}'." >&2
        if [[ -z "${current_branch}" ]]; then
            echo "  HEAD is detached or not a symbolic ref." >&2
        else
            echo "  Currently on: '${current_branch}'" >&2
        fi
        echo "  Move or delete ${STAGING}/ manually, then re-run." >&2
        exit 1
    fi

    echo "✓ ${STAGING}/ already exists as a worktree of '${BRANCH}'."
    echo "  Refreshing from origin..."
    git -C "${STAGING}" fetch --quiet origin "${BRANCH}" || true
    if git -C "${STAGING}" rev-parse "@{upstream}" >/dev/null 2>&1; then
        git -C "${STAGING}" reset --hard "@{upstream}"
    fi
    exit 0
fi

if [[ -e "${STAGING}" ]]; then
    echo "✗ ${STAGING}/ exists but is not a git worktree — refusing to clobber." >&2
    echo "  Move or delete it manually, then re-run." >&2
    exit 1
fi

# Try local branch first, then origin/<branch>, else fail with helpful guidance.
if git rev-parse --verify --quiet "refs/heads/${BRANCH}" >/dev/null; then
    echo "Creating worktree from local '${BRANCH}' branch..."
    git worktree add "${STAGING}" "${BRANCH}"
elif git rev-parse --verify --quiet "refs/remotes/origin/${BRANCH}" >/dev/null; then
    echo "Creating worktree tracking 'origin/${BRANCH}'..."
    git worktree add --track -b "${BRANCH}" "${STAGING}" "origin/${BRANCH}"
else
    echo "✗ Branch '${BRANCH}' does not exist locally or on origin." >&2
    echo
    echo "  This is expected on a fresh clone. The branch is bootstrapped" >&2
    echo "  by the FRB codegen workflow on the first codeowner push to master." >&2
    echo "  See docs/frb-flutter-migration.md for bootstrap instructions." >&2
    exit 1
fi

echo "✓ Staging worktree ready at ${STAGING}/."
echo "  Run 'just frb-generate' to populate it."

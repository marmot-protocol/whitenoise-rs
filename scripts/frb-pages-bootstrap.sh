#!/usr/bin/env bash
set -euo pipefail

# One-time setup of the gh-pages branch that will host the static pub
# registry for whitenoise_frb. Creates an orphan branch with the minimal
# scaffolding so subsequent `frb-update-index.sh` runs have a baseline.
#
# Run from the master worktree. Idempotent — re-running is a no-op once
# origin/${BRANCH} exists.
#
# Required env:
#   PUB_BASE_URL    e.g. https://marmot-protocol.github.io/whitenoise-rs
# Optional env:
#   BRANCH          Defaults to gh-pages
#   WORKTREE        Local worktree path (default: .gh-pages-bootstrap)
#
# Manual follow-up (cannot be scripted — it's a repo setting):
#   GitHub → Settings → Pages → Source = ${BRANCH} branch / root

PUB_BASE_URL="${PUB_BASE_URL:?PUB_BASE_URL is required (e.g. https://marmot-protocol.github.io/whitenoise-rs)}"
BRANCH="${BRANCH:-gh-pages}"
WORKTREE="${WORKTREE:-.gh-pages-bootstrap}"

if git rev-parse --verify --quiet "refs/remotes/origin/${BRANCH}" >/dev/null; then
    REMOTE_URL=$(git config --get remote.origin.url || echo "")
    REPO_PATH=$(echo "${REMOTE_URL}" | sed -E 's#.*github\.com[:/]##; s#\.git$##')
    echo "✓ origin/${BRANCH} already exists — no bootstrap needed."
    echo
    echo "  To verify Pages is enabled and serving from ${BRANCH}:"
    echo "    https://github.com/${REPO_PATH}/settings/pages"
    exit 0
fi

if [[ -e "${WORKTREE}" ]]; then
    echo "✗ ${WORKTREE}/ already exists. Move or delete it manually." >&2
    exit 1
fi

git worktree add --orphan -b "${BRANCH}" "${WORKTREE}"

# Disable Jekyll so directories like /api/ are served raw.
touch "${WORKTREE}/.nojekyll"

# Landing page so root URL doesn't 404 for humans browsing the registry.
cat > "${WORKTREE}/index.html" <<HTML
<!doctype html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <title>whitenoise-rs pub registry</title>
    <style>
        body { font-family: system-ui, sans-serif; max-width: 720px; margin: 2rem auto; padding: 0 1rem; line-height: 1.5; color: #222; }
        code { background: #f0f0f0; padding: 0.1em 0.3em; border-radius: 0.2em; }
        pre  { background: #f0f0f0; padding: 1rem; border-radius: 0.3em; overflow-x: auto; }
        a    { color: #0366d6; }
    </style>
</head>
<body>
    <h1>whitenoise-rs pub registry</h1>
    <p>Static Dart/Flutter Pub registry hosting <code>whitenoise_frb</code>, the Flutter Rust Bridge wrapper for <a href="https://github.com/marmot-protocol/whitenoise-rs">marmot-protocol/whitenoise-rs</a>.</p>
    <p>Consume from a Flutter app — pin to a specific published SHA:</p>
    <pre>dependencies:
  whitenoise_frb:
    hosted: ${PUB_BASE_URL}
    version: "0.0.0-dev+a1b2c3d"  # see registry for available versions</pre>
    <p>Package metadata: <a href="api/packages/whitenoise_frb"><code>api/packages/whitenoise_frb</code></a></p>
</body>
</html>
HTML

git -C "${WORKTREE}" add -A
git -C "${WORKTREE}" \
    -c user.email="frb-codegen-bot@marmot-protocol.org" \
    -c user.name="frb-codegen-bot" \
    commit -q -m "bootstrap: initialize ${BRANCH} for whitenoise_frb pub registry"

echo "✓ Created '${BRANCH}' branch at ${WORKTREE}/"
echo
echo "Next steps:"
echo "  1. Push:    git -C ${WORKTREE} push origin ${BRANCH}"
echo "  2. Enable:  GitHub → Settings → Pages → Source = ${BRANCH} / root"
echo "  3. Verify:  curl ${PUB_BASE_URL}/"
echo "  4. Cleanup: git worktree remove ${WORKTREE}"

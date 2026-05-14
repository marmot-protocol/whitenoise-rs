#!/usr/bin/env bash
set -euo pipefail

# Apply a freshly published version to a checked-out gh-pages worktree.
# Reads the tarball + version JSON produced by frb-publish.sh and mutates
# the static pub-registry index in-place. Does NOT commit or push — the
# caller (CI or local dry-run) owns the git operations so the same auth /
# retry pattern from the existing workflow can be reused.
#
# Required env:
#   PUBLISH_DIR     Output dir of frb-publish.sh (contains tarball + version JSON)
#   PACKAGE_NAME    e.g. whitenoise_frb
#   VERSION         Must match a file in PUBLISH_DIR
#   PAGES_WORKTREE  Path to a checked-out gh-pages worktree
# Optional env:
#   RETAIN_DEV_VERSIONS  Number of pre-release versions to keep (default: 20)
#
# Layout on gh-pages (Pub Repository Spec v2):
#   /.nojekyll
#   /api/packages/<name>                JSON: {name, latest, versions[]}
#   /packages/<name>/<version>.tar.gz

PUBLISH_DIR="${PUBLISH_DIR:?PUBLISH_DIR is required}"
PACKAGE_NAME="${PACKAGE_NAME:?PACKAGE_NAME is required}"
VERSION="${VERSION:?VERSION is required}"
PAGES_WORKTREE="${PAGES_WORKTREE:?PAGES_WORKTREE is required}"
RETAIN_DEV_VERSIONS="${RETAIN_DEV_VERSIONS:-20}"

TARBALL_SRC="${PUBLISH_DIR}/${PACKAGE_NAME}-${VERSION}.tar.gz"
VERSION_JSON="${PUBLISH_DIR}/${PACKAGE_NAME}-${VERSION}.version.json"

[[ -f "${TARBALL_SRC}" ]] || { echo "✗ Missing tarball: ${TARBALL_SRC}" >&2; exit 1; }
[[ -f "${VERSION_JSON}" ]] || { echo "✗ Missing version JSON: ${VERSION_JSON}" >&2; exit 1; }
[[ -d "${PAGES_WORKTREE}" ]] || { echo "✗ PAGES_WORKTREE doesn't exist: ${PAGES_WORKTREE}" >&2; exit 1; }
command -v jq >/dev/null || { echo "✗ jq required" >&2; exit 1; }

INDEX_FILE="${PAGES_WORKTREE}/api/packages/${PACKAGE_NAME}"
TARBALL_DEST="${PAGES_WORKTREE}/packages/${PACKAGE_NAME}/${VERSION}.tar.gz"

mkdir -p "$(dirname "${INDEX_FILE}")" "$(dirname "${TARBALL_DEST}")"
cp "${TARBALL_SRC}" "${TARBALL_DEST}"

NEW_ENTRY=$(cat "${VERSION_JSON}")
EXISTING_VERSIONS='[]'
if [[ -f "${INDEX_FILE}" ]]; then
    EXISTING_VERSIONS=$(jq '.versions' "${INDEX_FILE}")
fi

# Build the new index in a single jq pass:
#   1. Drop any existing entry for this version (idempotent republish).
#   2. Append the new entry.
#   3. Partition into stable / pre-release; keep all stable + last N pre-releases.
#   4. Sort chronologically by .published and set 'latest' to the newest.
#
# We sort by .published (not .version) so the index works regardless of the
# pre-release naming scheme — `0.0.0-dev+<sha>` versions have the same
# semver precedence and lex-sorting by sha is arbitrary.
NEW_INDEX=$(jq -n \
    --arg name "${PACKAGE_NAME}" \
    --argjson new "${NEW_ENTRY}" \
    --argjson existing "${EXISTING_VERSIONS}" \
    --argjson keep "${RETAIN_DEV_VERSIONS}" \
    '
    ($existing | map(select(.version != $new.version)) + [$new]) as $merged
    | (
        ($merged | map(select(.version | contains("-") | not))) +
        ($merged | map(select(.version | contains("-")))
                 | sort_by(.published)
                 | reverse | .[0:$keep] | reverse)
      ) as $kept
    | ($kept | sort_by(.published)) as $sorted
    | {name: $name, latest: $sorted[-1], versions: $sorted}
    ')

echo "${NEW_INDEX}" > "${INDEX_FILE}"

# Delete tarballs whose versions are no longer in the index.
KEPT_VERSIONS=$(echo "${NEW_INDEX}" | jq -r '.versions[].version')
shopt -s nullglob
for tarball in "${PAGES_WORKTREE}/packages/${PACKAGE_NAME}"/*.tar.gz; do
    base=$(basename "${tarball}")
    v="${base%.tar.gz}"
    if ! grep -qFx "${v}" <<<"${KEPT_VERSIONS}"; then
        rm -f "${tarball}"
        echo "  Pruned: ${v}"
    fi
done

# Ensure .nojekyll is present so GitHub Pages serves /api/ raw.
touch "${PAGES_WORKTREE}/.nojekyll"

echo "✓ Index updated: ${INDEX_FILE}"
echo "  New version:        ${VERSION}"
echo "  Versions in index:  $(jq '.versions | length' "${INDEX_FILE}")"
echo "  Latest:             $(jq -r '.latest.version' "${INDEX_FILE}")"

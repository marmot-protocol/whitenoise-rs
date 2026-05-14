#!/usr/bin/env bash
set -euo pipefail

# Package a populated FRB staging directory into a tarball plus a
# Pub-Repository-Spec-v2 version JSON, ready for upload to the static
# hosted pub registry. Producer-side only: doesn't push, just builds
# artifacts. The caller (CI or local dry-run) is responsible for
# delivering them to gh-pages via frb-update-index.sh.
#
# Required env:
#   STAGING_DIR     Populated package directory (pubspec.yaml at root)
#   VERSION         Semver string (e.g. 0.0.0-dev.20260514103200+a1b2c3d)
#   PUB_BASE_URL    e.g. https://marmot-protocol.github.io/whitenoise-rs
#   OUTPUT_DIR      Where to write the tarball + version JSON
# Optional env:
#   PACKAGE_NAME    Defaults to whitenoise_frb

STAGING_DIR="${STAGING_DIR:?STAGING_DIR is required}"
VERSION="${VERSION:?VERSION is required}"
PUB_BASE_URL="${PUB_BASE_URL:?PUB_BASE_URL is required}"
OUTPUT_DIR="${OUTPUT_DIR:?OUTPUT_DIR is required}"
PACKAGE_NAME="${PACKAGE_NAME:-whitenoise_frb}"

if [[ ! -f "${STAGING_DIR}/pubspec.yaml" ]]; then
    echo "✗ ${STAGING_DIR}/pubspec.yaml not found." >&2
    exit 1
fi

# Tools we depend on. CI image has these by default; document for local devs.
for tool in tar sha256sum jq python3; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        echo "✗ Required tool not found: ${tool}" >&2
        exit 1
    fi
done

mkdir -p "${OUTPUT_DIR}"
TARBALL="${OUTPUT_DIR}/${PACKAGE_NAME}-${VERSION}.tar.gz"
VERSION_JSON="${OUTPUT_DIR}/${PACKAGE_NAME}-${VERSION}.version.json"

# Stamp the published version into pubspec.yaml. Pub clients validate that
# the registry's `version` field matches the tarball's pubspec.version;
# the template ships `version: 0.0.1` so without this rewrite every
# publish lists as 0.0.0-dev+<sha> on the index but is internally 0.0.1,
# and the resolver rejects "0.0.0-dev+<sha> doesn't match any versions".
sed -i "s/^version: .*\$/version: ${VERSION}/" "${STAGING_DIR}/pubspec.yaml"

# Tar from inside STAGING_DIR so pubspec.yaml lives at the archive root.
# Exclude transient/local artifacts that must never ship.
tar -czf "${TARBALL}" \
    -C "${STAGING_DIR}" \
    --exclude='.dart_tool' \
    --exclude='.packages' \
    --exclude='pubspec.lock' \
    --exclude='target' \
    --exclude='build' \
    --exclude='.git' \
    --exclude='.github' \
    .

SHA256=$(sha256sum "${TARBALL}" | awk '{print $1}')
ARCHIVE_URL="${PUB_BASE_URL%/}/packages/${PACKAGE_NAME}/${VERSION}.tar.gz"
PUBLISHED_AT=$(date -u +%Y-%m-%dT%H:%M:%S.000Z)

# Pub Repository Spec v2 requires the pubspec inlined as JSON in the
# version entry so clients can resolve without an extra request.
PUBSPEC_JSON=$(python3 -c "
import json, yaml
with open('${STAGING_DIR}/pubspec.yaml') as f:
    print(json.dumps(yaml.safe_load(f)))
")

jq -n \
    --arg version "${VERSION}" \
    --arg archive_url "${ARCHIVE_URL}" \
    --arg archive_sha256 "${SHA256}" \
    --argjson pubspec "${PUBSPEC_JSON}" \
    --arg published "${PUBLISHED_AT}" \
    '{
        version: $version,
        archive_url: $archive_url,
        archive_sha256: $archive_sha256,
        pubspec: $pubspec,
        published: $published
    }' \
    > "${VERSION_JSON}"

echo "✓ Published ${PACKAGE_NAME} ${VERSION}"
echo "  Tarball:  ${TARBALL}"
echo "  SHA-256:  ${SHA256}"
echo "  Version:  ${VERSION_JSON}"

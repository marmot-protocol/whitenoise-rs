#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
ENV_DIR="$REPO_ROOT/dev/transponder"
ENV_FILE="$ENV_DIR/.env"
ENV_EXAMPLE="$ENV_DIR/.env.example"

mkdir -p "$ENV_DIR"

if [ ! -f "$ENV_EXAMPLE" ]; then
    echo "Missing template env file: $ENV_EXAMPLE" >&2
    exit 1
fi

if [ -f "$ENV_FILE" ] && grep -Eq '^TRANSPONDER_SERVER_PRIVATE_KEY=[0-9a-f]{64}$' "$ENV_FILE"; then
    echo "Using existing Transponder env file at $ENV_FILE"
    exit 0
fi

cp "$ENV_EXAMPLE" "$ENV_FILE"

PRIVATE_KEY=$(openssl rand -hex 32)
TMP_FILE=$(mktemp)

awk -v private_key="$PRIVATE_KEY" '
BEGIN { replaced = 0 }
/^TRANSPONDER_SERVER_PRIVATE_KEY=/ {
    print "TRANSPONDER_SERVER_PRIVATE_KEY=" private_key
    replaced = 1
    next
}
{ print }
END {
    if (!replaced) {
        print "TRANSPONDER_SERVER_PRIVATE_KEY=" private_key
    }
}
' "$ENV_FILE" > "$TMP_FILE"

mv "$TMP_FILE" "$ENV_FILE"
chmod 600 "$ENV_FILE"

echo "Generated a local-only Transponder env file at $ENV_FILE"

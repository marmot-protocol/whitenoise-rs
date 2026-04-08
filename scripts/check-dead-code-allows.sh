#!/bin/bash

set -euo pipefail

echo "🔍 Checking for #[allow(dead_code)] annotations..."

matches=$(grep -rn '#\[allow(dead_code)\]' src/ --include='*.rs' || true)

if [ -n "$matches" ]; then
    echo "❌ Found #[allow(dead_code)] annotations:"
    echo "$matches"
    echo
    echo "Remove #[allow(dead_code)] and either delete the dead code or gate it with #[cfg(test)]."
    exit 1
fi

echo "✅ No #[allow(dead_code)] annotations found"
echo

#!/bin/bash

set -euo pipefail

echo "🔍 Checking for allow(dead_code) annotations..."

# Matches all forms: #[allow(dead_code)], #![allow(dead_code)],
# #[cfg_attr(..., allow(dead_code))], #[allow(unused, dead_code)], etc.
matches=$(grep -rn -E 'allow\s*\(\s*([^)]*,\s*)?dead_code\s*(,\s*[^)]*)?\)' src/ --include='*.rs' || true)

if [ -n "$matches" ]; then
    echo "❌ Found #[allow(dead_code)] annotations:"
    echo "$matches"
    echo
    echo "Remove #[allow(dead_code)] and either delete the dead code or gate it with #[cfg(test)]."
    exit 1
fi

echo "✅ No #[allow(dead_code)] annotations found"
echo

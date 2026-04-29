#!/bin/bash

set -euo pipefail

echo "===================="
echo "🚀 Running all checks"
echo "===================="
echo

./scripts/check-fmt.sh check
./scripts/check-docs.sh
./scripts/check-clippy.sh
./scripts/check-dead-code-allows.sh

echo "===================="
echo "✅ All checks passed!"
echo "===================="

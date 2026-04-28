#!/usr/bin/env bash
# Verify that db_migrations/ is frozen after the Rust migration framework landed.
# No files may be added, removed, or modified. All new schema changes must use
# Rust migrations in src/whitenoise/database/rust_migrations/.
set -euo pipefail

BASELINE="db_migrations/.frozen.sha256"

if [ ! -f "$BASELINE" ]; then
    echo "ERROR: Frozen-migrations baseline not found at $BASELINE"
    exit 1
fi

# Check for added or removed files by comparing the file list.
expected_files=$(awk '{print $2}' "$BASELINE" | sort)
actual_files=$(find db_migrations -maxdepth 1 -name '*.sql' -type f | sort)

if [ "$expected_files" != "$actual_files" ]; then
    echo "ERROR: db_migrations/ file list does not match the frozen baseline."
    echo ""
    added=$(comm -13 <(echo "$expected_files") <(echo "$actual_files"))
    removed=$(comm -23 <(echo "$expected_files") <(echo "$actual_files"))
    [ -n "$added" ] && echo "  Added:   $added"
    [ -n "$removed" ] && echo "  Removed: $removed"
    echo ""
    echo "  The SQLx migration system has been replaced by Rust migrations."
    echo "  Do NOT add, remove, or modify files in db_migrations/."
    echo "  Instead, add a new Rust migration in:"
    echo "    src/whitenoise/database/rust_migrations/global/"
    echo "    src/whitenoise/database/rust_migrations/local/"
    echo ""
    exit 1
fi

# Check for modified files via SHA-256 checksums.
if ! shasum -a 256 -c "$BASELINE" --status 2>/dev/null; then
    echo "ERROR: db_migrations/ checksums do not match the frozen baseline."
    echo ""
    echo "  Modified files:"
    # Re-run without --status to show which files failed.
    shasum -a 256 -c "$BASELINE" 2>/dev/null | grep -v ': OK$' | sed 's/^/    /'
    echo ""
    echo "  The SQLx migration system has been replaced by Rust migrations."
    echo "  Do NOT add, remove, or modify files in db_migrations/."
    echo "  Instead, add a new Rust migration in:"
    echo "    src/whitenoise/database/rust_migrations/global/"
    echo "    src/whitenoise/database/rust_migrations/local/"
    echo ""
    exit 1
fi

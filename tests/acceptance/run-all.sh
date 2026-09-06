#!/usr/bin/env bash
# Runs the full acceptance suite against the built binaries.
#   ACYCLIC_BIN / ACYCLIC_QUAL   override binary paths (default: target/debug)
#   Individual scripts also run standalone.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

FAILED=0
SCRIPTS=(journey.sh soak.sh crash.sh latency.sh forks.sh safe-mode.sh)
# The live Claude Code session test needs the claude CLI + credentials and
# costs a model session; opt in with ACYCLIC_E2E=1.
[ "${ACYCLIC_E2E:-0}" = "1" ] && SCRIPTS+=(claude-e2e.sh)
for script in "${SCRIPTS[@]}"; do
  echo "=== $script"
  if ! bash "$HERE/$script"; then
    FAILED=$((FAILED + 1))
  fi
done

if [ "$FAILED" -ne 0 ]; then
  echo "acceptance: $FAILED script(s) failed" >&2
  exit 1
fi
echo "acceptance: all green"

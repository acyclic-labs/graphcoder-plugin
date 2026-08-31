#!/usr/bin/env bash
# Runs the full acceptance suite against the built binaries.
#   ACYCLIC_BIN / ACYCLIC_QUAL   override binary paths (default: target/debug)
#   Individual scripts also run standalone.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

FAILED=0
for script in journey.sh soak.sh crash.sh latency.sh; do
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

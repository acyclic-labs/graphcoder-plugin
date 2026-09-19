#!/usr/bin/env bash
# Runs the full acceptance suite against the built binaries.
#   ACYCLIC_BIN / ACYCLIC_QUAL   override binary paths (default: target/debug)
#   Individual scripts also run standalone.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

FAILED=0
SCRIPTS=(journey.sh timeline.sh exclusions.sh growth.sh soak.sh crash.sh latency.sh speculation.sh forks.sh forks-scale.sh merge.sh safe-mode.sh mcp-e2e.sh pydantic-e2e.sh)
# The live host-session tests need their CLI + credentials and cost a model
# session each; opt in with ACYCLIC_E2E=1. (mcp-e2e.sh and pydantic-e2e.sh are
# not among them: one drives `acyclic mcp` with a scripted client, the other a
# real Pydantic AI agent on a scripted model; neither needs a host app or
# credentials. pydantic-e2e.sh skips when python3 lacks pydantic-ai.)
[ "${ACYCLIC_E2E:-0}" = "1" ] && SCRIPTS+=(claude-e2e.sh claude-merge-e2e.sh codex-e2e.sh cursor-e2e.sh mcp-clients-e2e.sh)
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

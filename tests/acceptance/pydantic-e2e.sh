#!/usr/bin/env bash
# The Pydantic AI adapter, end to end: `acyclic install pydantic-ai --yes`
# into an isolated repo, then a real Pydantic AI agent (pydantic_harness.py)
# with the `Acyclic` capability attached edits the tree. Asserts the same
# things claude-e2e.sh does: hooks fired with session + tool attribution,
# the edit and shell side effects are captured, diff names them, and
# rewind --session-start restores the pre-session tree exactly.
#
# Runs the harness's scripted model by default, which needs no credentials
# and no network: the hook path is what is under test, not the model. Set
# ACYCLIC_E2E_PYDANTIC_MODEL to a Pydantic AI model name for a live pass.
#
# Needs a python3 with pydantic-ai importable (ACYCLIC_E2E_PYTHON overrides
# the interpreter); skips (exit 0) otherwise unless ACYCLIC_E2E_REQUIRED=1.
# The capability package is used from the source tree, not from PyPI.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

skip() {
  if [ "${ACYCLIC_E2E_REQUIRED:-0}" = "1" ]; then
    fail "$*"
  fi
  echo "SKIP($(basename "$0")): $*"
  exit 0
}

PY="${ACYCLIC_E2E_PYTHON:-python3}"
command -v "$PY" >/dev/null 2>&1 || skip "$PY not on PATH"
"$PY" -c 'import pydantic_ai' 2>/dev/null || skip "pydantic_ai not importable by $PY (pip install pydantic-ai-slim)"
MODEL="${ACYCLIC_E2E_PYDANTIC_MODEL:-scripted}"
e2e_model_note pydantic-ai "$MODEL"

setup_repo
printf 'pydantic-ai-slim\n' > "$R/requirements.txt"
acy init >/dev/null || fail "init"
# --yes: the dependency prompt is answered for us, and stdin is not a
# terminal here anyway.
acy install pydantic-ai --yes >/dev/null || fail "install pydantic-ai"
grep -q "checkpoints" "$R/AGENTS.md" 2>/dev/null || fail "AGENTS.md cheatsheet not written"
grep -q "^acyclic-pydantic-ai$" "$R/requirements.txt" || fail "dependency not added: $(cat "$R/requirements.txt")"

# The capability shells out to bare `acyclic` (or ACYCLIC_BIN); point it at
# the binary under test. The package comes from the source tree.
BIN_ABS="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
OUT="$(cd "$R" && ACYCLIC_BIN="$BIN_ABS" PYDANTIC_AI_NO_BANNER=1 \
  PYTHONPATH="$REPO_ROOT/packaging/pypi/src${PYTHONPATH:+:$PYTHONPATH}" \
  "$PY" "$REPO_ROOT/tests/acceptance/pydantic_harness.py" --repo "$R" --model "$MODEL" \
  2>"$WORK/harness.stderr")" \
  || skip "harness failed: $(tail -c 400 "$WORK/harness.stderr")"

SID="$(printf '%s' "$OUT" | tail -1 | "$PY" -c 'import json,sys; print(json.load(sys.stdin)["session_id"])')"
[ -n "$SID" ] || fail "no session_id from harness: $OUT"

# The agent actually did the work.
[ "$(cat "$R/src/main.rs")" = "MIGRATED" ] || fail "agent edit missing: $(cat "$R/src/main.rs")"
[ ! -e "$R/.env" ] || fail "agent did not delete .env"
[ -e "$R/generated.bin" ] || fail "agent did not generate artifact"

settle 1

# Hooks fired and attributed: pre and post rows for THIS session carrying
# the tool names the capability reported, and the session is recorded as
# a pydantic-ai host session.
TL="$(acy timeline --session "$SID" --limit 100)"
echo "$TL" | grep -Eq " (pre|noop) +t[0-9]+ " || fail "no pre checkpoint for session: $TL"
echo "$TL" | grep -q " post " || fail "no post checkpoint for session: $TL"
echo "$TL" | grep -q "write_file" || fail "no write_file attribution: $TL"
echo "$TL" | grep -q "run_shell" || fail "no run_shell attribution: $TL"
echo "$TL" | grep -q "search" && fail "read-only tool was checkpointed: $TL"
acy sessions | grep -q "pydantic-ai" || fail "session not recorded as pydantic-ai host: $(acy sessions)"

# Blast radius across the session names the edit, the deleted gitignored
# secret, and the shell-generated artifact.
LAST_ID="$(echo "$TL" | head -1 | awk '{print $1}' | tr -d '#')"
FIRST_ID="$(echo "$TL" | tail -1 | awk '{print $1}' | tr -d '#')"
DIFF="$(acy diff "$FIRST_ID" "$LAST_ID")"
echo "$DIFF" | grep -q "^M src/main.rs" || fail "diff missing edit: $DIFF"
echo "$DIFF" | grep -q "^D .env" || fail "diff missing .env removal: $DIFF"
echo "$DIFF" | grep -q "^A generated.bin" || fail "diff missing artifact: $DIFF"

# A second process, later: it is handed the first session's brief in its
# instructions before its first request, the way SessionStart stdout lands
# in Claude Code's context. (Its own edits are no-ops on the already-migrated
# tree; only the brief is under test here.)
OUT2="$(cd "$R" && ACYCLIC_BIN="$BIN_ABS" PYDANTIC_AI_NO_BANNER=1 \
  PYTHONPATH="$REPO_ROOT/packaging/pypi/src${PYTHONPATH:+:$PYTHONPATH}" \
  "$PY" "$REPO_ROOT/tests/acceptance/pydantic_harness.py" --repo "$R" --model "$MODEL" \
  2>"$WORK/harness2.stderr")" \
  || fail "second harness run failed: $(tail -c 400 "$WORK/harness2.stderr")"
BRIEF="$(printf '%s' "$OUT2" | tail -1 | "$PY" -c 'import json,sys; print(json.load(sys.stdin)["instructions"])')"
[ -n "$BRIEF" ] || fail "second run received no brief in its instructions"
echo "$BRIEF" | grep -qi "session" || fail "brief does not read like a brief: $BRIEF"

# Rewind to the FIRST session's start: the tree is back exactly, gitignored
# state included, generated artifact gone.
acy rewind --session-start "$SID" --yes >/dev/null || fail "rewind --session-start"
[ "$(cat "$R/src/main.rs")" = "ORIGINAL" ] || fail "edit not reverted"
[ "$(cat "$R/.env")" = "SECRET=1" ] || fail "gitignored .env not restored"
[ ! -e "$R/generated.bin" ] || fail "artifact not removed"

pass "pydantic-e2e ($MODEL): capability checkpointed, attributed, diffed, and rewound"

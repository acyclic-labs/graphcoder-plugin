#!/usr/bin/env bash
# The Codex analogue of claude-e2e.sh: a real Codex CLI session in an
# isolated repo with the adapter installed end-to-end via
# `acyclic install codex`.
# Asserts: hooks fire (pre/post checkpoints with session + tool
# attribution), the agent's edit and Bash side effects are captured,
# blast-radius diff names them, and rewind --session-start restores the
# pre-session tree exactly (gitignored state included).
#
# Needs the `codex` CLI and credentials; skips (exit 0) when absent unless
# ACYCLIC_E2E_REQUIRED=1. Costs one short model session.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

skip() {
  if [ "${ACYCLIC_E2E_REQUIRED:-0}" = "1" ]; then
    fail "$*"
  fi
  echo "SKIP($(basename "$0")): $*"
  exit 0
}

command -v codex >/dev/null 2>&1 || skip "codex CLI not on PATH"

# Codex's exec flags have moved fast across releases; check the installed
# CLI actually has what this script depends on rather than letting a
# missing flag surface as an opaque mid-session failure.
EXEC_HELP="$(codex exec --help 2>&1 || true)"
require_flag "$EXEC_HELP" --skip-git-repo-check codex
require_flag "$EXEC_HELP" --dangerously-bypass-approvals-and-sandbox codex
require_flag "$EXEC_HELP" --dangerously-bypass-hook-trust codex
require_flag "$EXEC_HELP" --json codex

setup_repo
acy init >/dev/null || fail "init"
acy install codex >/dev/null || fail "install codex"
[ -f "$R/.codex/hooks.json" ] || fail "hooks.json not written"
grep -q "acyclic hook pre-tool" "$R/.codex/hooks.json" || fail "hooks not merged"

# The hooks invoke bare `acyclic` from the session's cwd; point PATH at the
# binary under test so the live session exercises exactly what we built.
BIN_DIR="$(cd "$(dirname "$BIN")" && pwd)"

PROMPT='Do exactly these three steps, in order, with no other file or shell operations and no commentary:
1. Overwrite src/main.rs with exactly the single line: MIGRATED
2. Run this exact bash command: rm .env
3. Run this exact bash command: head -c 4096 /dev/zero > generated.bin'

# --json streams JSONL events including the session id; --skip-git-repo-check
# because the acceptance repo is a plain temp git init; --dangerously-*
# flags are needed for a non-interactive, unattended run against a repo
# whose project-local hooks.json would otherwise require an interactive
# trust prompt. with_timeout (common.sh) stands in for GNU `timeout`, which
# stock macOS ships neither as `timeout` nor `gtimeout`.
OUT="$(cd "$R" && PATH="$BIN_DIR:$PATH" with_timeout 180 codex exec "$PROMPT" \
  --skip-git-repo-check \
  --dangerously-bypass-approvals-and-sandbox \
  --dangerously-bypass-hook-trust \
  --json 2>"$WORK/codex.stderr")" \
  || skip "codex session failed or timed out: $(tail -c 300 "$WORK/codex.stderr")"

SID="$(printf '%s' "$OUT" | grep -o '"session_id"[[:space:]]*:[[:space:]]*"[^"]*"' \
  | head -1 | sed 's/.*"\([^"]*\)"$/\1/')"
[ -n "$SID" ] || skip "no session_id in codex output: $(printf '%s' "$OUT" | head -c 300)"

# The agent actually did the work.
[ "$(cat "$R/src/main.rs")" = "MIGRATED" ] || fail "agent edit missing: $(cat "$R/src/main.rs")"
[ ! -e "$R/.env" ] || fail "agent did not delete .env"
[ -e "$R/generated.bin" ] || fail "agent did not generate artifact"

settle 1

# Hooks fired and attributed: the timeline for THIS session has pre and post
# rows carrying the tool names the host reported.
TL="$(acy timeline --session "$SID" --limit 100)"
echo "$TL" | grep -q " pre " || fail "no pre checkpoint for session: $TL"
echo "$TL" | grep -q " post " || fail "no post checkpoint for session: $TL"
echo "$TL" | grep -Eq "Write|Edit" || fail "no Write/Edit attribution: $TL"
echo "$TL" | grep -q "Bash" || fail "no Bash attribution: $TL"

# Blast radius across the session (earliest -> latest checkpoint) names the
# edit, the deleted gitignored secret, and the Bash-generated artifact.
LAST_ID="$(echo "$TL" | head -1 | awk '{print $1}' | tr -d '#')"
FIRST_ID="$(echo "$TL" | tail -1 | awk '{print $1}' | tr -d '#')"
DIFF="$(acy diff "$FIRST_ID" "$LAST_ID")"
echo "$DIFF" | grep -q "^M src/main.rs" || fail "diff missing edit: $DIFF"
echo "$DIFF" | grep -q "^D .env" || fail "diff missing .env removal: $DIFF"
echo "$DIFF" | grep -q "^A generated.bin" || fail "diff missing artifact: $DIFF"

# Rewind to the session start: the tree is back exactly, gitignored state
# included, generated artifact gone.
acy rewind --session-start "$SID" --yes >/dev/null || fail "rewind --session-start"
[ "$(cat "$R/src/main.rs")" = "ORIGINAL" ] || fail "edit not reverted"
[ "$(cat "$R/.env")" = "SECRET=1" ] || fail "gitignored .env not restored"
[ ! -e "$R/generated.bin" ] || fail "artifact not removed"

pass "codex-e2e: live session checkpointed, attributed, diffed, and rewound"

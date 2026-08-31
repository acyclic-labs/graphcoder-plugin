#!/usr/bin/env bash
# The Phase 3 exit gate, live: a real Claude Code session in an isolated repo
# with the adapter installed end-to-end via `acyclic install claude-code`.
# Asserts: hooks fire (pre/post checkpoints with session + tool attribution),
# the agent's edit and Bash side effects are captured, blast-radius diff names
# them, and rewind --session-start restores the pre-session tree exactly
# (gitignored state included).
#
# Needs the `claude` CLI and credentials; skips (exit 0) when absent unless
# ACYCLIC_E2E_REQUIRED=1. Costs one short model session.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

skip() {
  if [ "${ACYCLIC_E2E_REQUIRED:-0}" = "1" ]; then
    fail "$*"
  fi
  echo "SKIP($(basename "$0")): $*"
  exit 0
}

command -v claude >/dev/null 2>&1 || skip "claude CLI not on PATH"

setup_repo
acy init >/dev/null || fail "init"
acy install claude-code >/dev/null || fail "install claude-code"
[ -f "$R/.claude/settings.json" ] || fail "settings.json not written"
grep -q "acyclic hook pre-tool" "$R/.claude/settings.json" || fail "hooks not merged"

# The hooks invoke bare `acyclic` from the session's cwd; point PATH at the
# binary under test so the live session exercises exactly what we built.
BIN_DIR="$(cd "$(dirname "$BIN")" && pwd)"

PROMPT='Do exactly these three steps, in order, with no other file or shell operations and no commentary:
1. Use the Write tool to overwrite src/main.rs with exactly the single line: MIGRATED
2. Run this exact bash command: rm .env
3. Run this exact bash command: head -c 4096 /dev/zero > generated.bin'

# --settings applies the repo hook wiring explicitly (no trust-prompt
# dependence); --output-format json hands us the session id for attribution
# and rewind targeting.
OUT="$(cd "$R" && PATH="$BIN_DIR:$PATH" claude -p "$PROMPT" \
  --settings .claude/settings.json \
  --dangerously-skip-permissions \
  --output-format json 2>"$WORK/claude.stderr")" \
  || skip "claude session failed: $(tail -c 300 "$WORK/claude.stderr")"

SID="$(printf '%s' "$OUT" | grep -o '"session_id"[[:space:]]*:[[:space:]]*"[^"]*"' \
  | head -1 | sed 's/.*"\([^"]*\)"$/\1/')"
[ -n "$SID" ] || fail "no session_id in claude output"

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

pass "claude-e2e: live session checkpointed, attributed, diffed, and rewound"

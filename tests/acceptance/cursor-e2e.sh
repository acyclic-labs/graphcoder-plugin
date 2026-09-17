#!/usr/bin/env bash
# The Cursor analogue of claude-e2e.sh: a real cursor-agent session in an
# isolated repo with the adapter installed end-to-end via
# `acyclic install cursor`.
# Asserts: hooks fire (pre/post checkpoints attributed to the session), the
# agent's edit and Bash side effects are captured, blast-radius diff names
# them, and rewind --session-start restores the pre-session tree exactly
# (gitignored state included).
#
# Needs the `cursor-agent` CLI and credentials; skips (exit 0) when absent
# unless ACYCLIC_E2E_REQUIRED=1. Costs one short model session.
#
# Cursor's own session id (reported here as `session_id` in --output-format
# json) is the same identifier its beforeShellExecution/afterFileEdit hooks
# send as `conversation_id` — that's the fallback `hook::Payload::session()`
# resolves to `session_id` for attribution (Cursor's own `sessionStart`
# event does send `session_id` directly). If Cursor ever splits these, the
# timeline/diff assertions below will fail loudly rather than silently
# mis-attribute.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

skip() {
  if [ "${ACYCLIC_E2E_REQUIRED:-0}" = "1" ]; then
    fail "$*"
  fi
  echo "SKIP($(basename "$0")): $*"
  exit 0
}

command -v cursor-agent >/dev/null 2>&1 || skip "cursor-agent CLI not on PATH"

# cursor-agent's flags have moved fast across releases; check the installed
# CLI actually has what this script depends on rather than letting a
# missing flag surface as an opaque mid-session failure.
AGENT_HELP="$(cursor-agent --help 2>&1 || true)"
require_flag "$AGENT_HELP" --output-format cursor-agent
require_flag "$AGENT_HELP" --force cursor-agent
require_flag "$AGENT_HELP" --trust cursor-agent
if [ -n "$E2E_CURSOR_MODEL" ]; then
  require_flag "$AGENT_HELP" --model cursor-agent
fi
e2e_model_note cursor-agent "$E2E_CURSOR_MODEL"

setup_repo
acy init >/dev/null || fail "init"
acy install cursor >/dev/null || fail "install cursor"
[ -f "$R/.cursor/hooks.json" ] || fail "hooks.json not written"
grep -q "acyclic hook pre-tool" "$R/.cursor/hooks.json" || fail "hooks not merged"

# The hooks invoke bare `acyclic` from the session's cwd; point PATH at the
# binary under test so the live session exercises exactly what we built.
BIN_DIR="$(cd "$(dirname "$BIN")" && pwd)"

PROMPT='Do exactly these three steps, in order, with no other file or shell operations and no commentary:
1. Overwrite src/main.rs with exactly the single line: MIGRATED
2. Run this exact bash command: rm .env
3. Run this exact bash command: head -c 4096 /dev/zero > generated.bin'

# --trust skips the interactive workspace-trust prompt that would otherwise
# block a checked-in .cursor/hooks.json from loading; --force/--yolo skips
# per-command approval so the scripted run doesn't hang. with_timeout
# (common.sh) stands in for GNU `timeout`, which stock macOS ships neither
# as `timeout` nor `gtimeout`.
OUT="$(cd "$R" && PATH="$BIN_DIR:$PATH" with_timeout 180 cursor-agent -p "$PROMPT" \
  ${CURSOR_MODEL_ARGS[@]+"${CURSOR_MODEL_ARGS[@]}"} \
  --output-format json \
  --force \
  --trust 2>"$WORK/cursor.stderr")" \
  || skip "cursor-agent session failed or timed out: $(tail -c 300 "$WORK/cursor.stderr")"

SID="$(printf '%s' "$OUT" | grep -o '"session_id"[[:space:]]*:[[:space:]]*"[^"]*"' \
  | head -1 | sed 's/.*"\([^"]*\)"$/\1/' || true)"
[ -n "$SID" ] || skip "no session_id in cursor-agent output: $(printf '%s' "$OUT" | head -c 300)"

# The agent actually did the work.
[ "$(cat "$R/src/main.rs")" = "MIGRATED" ] || fail "agent edit missing: $(cat "$R/src/main.rs")"
[ ! -e "$R/.env" ] || fail "agent did not delete .env"
[ -e "$R/generated.bin" ] || fail "agent did not generate artifact"

settle 1

# Hooks fired and attributed: the timeline for THIS session has pre and post
# rows carrying the tool names the host reported. A pre row lands as `noop`
# when the tree is already captured (the idle-timer auto checkpoint usually
# has it by the time the first tool runs); the hook still fired and is
# attributed, which is what this asserts.
TL="$(acy timeline --session "$SID" --limit 100)"
echo "$TL" | grep -Eq " (pre|noop) +t[0-9]+ " || fail "no pre checkpoint for session: $TL"
echo "$TL" | grep -q " post " || fail "no post checkpoint for session: $TL"
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

pass "cursor-e2e: live session checkpointed, attributed, diffed, and rewound"

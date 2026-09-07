#!/usr/bin/env bash
# Launch 2 (plans/02-timeline.md) acceptance, scripted end to end through the
# hook contract a host would use:
#   1. any checkpoint resolves to (session, turn, prompt) and back;
#   2. a new session's start brief names the last session's end state and
#      the branches it abandoned, in < 1KB;
#   3. one file restored from an old checkpoint, rest of the tree untouched;
#   4. all of it survives a daemon restart and a kill -9.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

hook() {
  # $1 event, $2 JSON payload (what Claude Code writes on the hook's stdin)
  printf '%s' "$2" | "$BIN" --repo "$R" hook "$1"
}

setup_repo
printf 'shared\n' > "$R/README.md"
acy init >/dev/null || fail "init"

# ---- Session A: two turns, the second one rewound (an abandoned branch).
SA=sess-A-0123456789
hook session-start "{\"session_id\":\"$SA\",\"source\":\"startup\"}" >/dev/null
hook user-prompt "{\"session_id\":\"$SA\",\"prompt\":\"turn one:   add   the\\nJWT refactor\"}"
hook pre-tool "{\"session_id\":\"$SA\",\"tool_name\":\"Edit\",\"tool_use_id\":\"t1\"}"
printf 'JWT v1\n' > "$R/src/main.rs"
settle 0.3
hook post-tool "{\"session_id\":\"$SA\",\"tool_name\":\"Edit\",\"tool_use_id\":\"t1\"}"
settle 0.6
T1_END="$(acy timeline --session "$SA" --turn 1 --limit 1 | awk '{print $1}' | tr -d '#')"
[ -n "$T1_END" ] || fail "turn 1 has no checkpoints"

hook user-prompt "{\"session_id\":\"$SA\",\"prompt\":\"turn two: try approach 2 with a different config\"}"
hook pre-tool "{\"session_id\":\"$SA\",\"tool_name\":\"Bash\",\"tool_use_id\":\"t2\"}"
printf 'approach 2\n' > "$R/src/main.rs"
printf 'cfg=2\n' > "$R/config.toml"
settle 0.3
hook post-tool "{\"session_id\":\"$SA\",\"tool_name\":\"Bash\",\"tool_use_id\":\"t2\"}"
settle 0.6

# 1. checkpoint -> (session, turn, prompt)
T2_END="$(acy timeline --session "$SA" --turn 2 --limit 1 | awk '{print $1}' | tr -d '#')"
SHOW="$(acy show "$T2_END")"
echo "$SHOW" | grep -q "session:     $SA" || fail "show lacks session: $SHOW"
echo "$SHOW" | grep -q "turn:        2" || fail "show lacks turn: $SHOW"
echo "$SHOW" | grep -q 'prompt:      "turn two: try approach 2' || fail "show lacks prompt: $SHOW"
# ... and (session, turn) -> checkpoints, with the prompt whitespace-normalized
TURNS="$(acy turns --session "$SA")"
echo "$TURNS" | grep -q 't1 .*"turn one: add the JWT refactor"' || fail "turn 1 row: $TURNS"
echo "$TURNS" | grep -q "t2 .*#.*\"turn two" || fail "turn 2 row: $TURNS"
TL="$(acy timeline --session "$SA" --turn 2)"
echo "$TL" | grep -q "#$T2_END " || fail "timeline --turn 2 lacks its own checkpoint: $TL"
echo "$TL" | grep -q "#$T1_END " && fail "timeline --turn 2 leaked turn 1: $TL"

# What turn 2 changed, exactly.
D2="$(acy diff --turn 2 --session "$SA")"
echo "$D2" | grep -q "^M src/main.rs" || fail "diff --turn 2 missing edit: $D2"
echo "$D2" | grep -q "^A config.toml" || fail "diff --turn 2 missing add: $D2"
echo "$D2" | grep -q "README" && fail "diff --turn 2 leaked unrelated file: $D2"

# Abandon approach 2: rewind to the end of turn 1, then finish with turn 3.
acy rewind "$T1_END" --yes >/dev/null || fail "rewind"
[ "$(cat "$R/src/main.rs")" = "JWT v1" ] || fail "rewind did not restore turn 1 state"
hook user-prompt "{\"session_id\":\"$SA\",\"prompt\":\"turn three: approach 3, tests passing\"}"
hook pre-tool "{\"session_id\":\"$SA\",\"tool_name\":\"Edit\",\"tool_use_id\":\"t3\"}"
printf 'approach 3\n' > "$R/src/main.rs"
settle 0.3
hook post-tool "{\"session_id\":\"$SA\",\"tool_name\":\"Edit\",\"tool_use_id\":\"t3\"}"
settle 0.6
A_END="$(acy timeline --session "$SA" --turn 3 --limit 1 | awk '{print $1}' | tr -d '#')"
hook session-end "{\"session_id\":\"$SA\"}"

# ---- 4a. Daemon restart: the timeline, turns, and session survive.
acy stop >/dev/null || fail "stop"
sleep 0.5
acy turns --session "$SA" | grep -q "t3 " || fail "turns lost across restart"
acy sessions | grep -q "sess-A-0" || fail "session lost across restart"

# ---- 2. Session B starts: the brief names A's end state + abandoned branch.
SB=sess-B-0123456789
BRIEF="$(hook session-start "{\"session_id\":\"$SB\",\"source\":\"startup\"}")"
echo "$BRIEF" | grep -q "last session sess-A-0" || fail "brief lacks last session: $BRIEF"
echo "$BRIEF" | grep -q "checkpoint #$A_END" || fail "brief lacks end checkpoint #$A_END: $BRIEF"
echo "$BRIEF" | grep -q "turn 3: \"turn three" || fail "brief lacks end turn/prompt: $BRIEF"
echo "$BRIEF" | grep -q "1 abandoned branch" || fail "brief lacks abandoned branch: $BRIEF"
echo "$BRIEF" | grep -q "turn 2 \"turn two" || fail "abandoned branch not attributed to turn 2: $BRIEF"
echo "$BRIEF" | grep -q "rewound to #$T1_END" || fail "abandoned branch target wrong: $BRIEF"
echo "$BRIEF" | grep -q "tree unchanged since then" || fail "brief reports drift: $BRIEF"
BYTES="$(printf '%s' "$BRIEF" | wc -c | tr -d ' ')"
[ "$BYTES" -lt 1024 ] || fail "brief is $BYTES bytes (must be < 1KB)"
# A compaction restart gets no brief (context already has it).
COMPACT="$(hook session-start "{\"session_id\":\"$SB\",\"source\":\"compact\"}")"
[ -z "$COMPACT" ] || fail "compact session-start printed a brief: $COMPACT"
# The same brief on demand, structured.
acy brief --current "$SB" --json | grep -q '"from_checkpoint"' || fail "brief --json lacks structure"

# ---- 3. Single-file restore from the abandoned approach, rest untouched.
printf 'other work\n' > "$R/README.md"
settle 0.3
acy checkpoint --wait --session-id "$SB" -m "B edits README" >/dev/null || fail "checkpoint"
acy restore "$T2_END" src/main.rs >/dev/null || fail "restore"
[ "$(cat "$R/src/main.rs")" = "approach 2" ] || fail "restore did not bring back approach 2: $(cat "$R/src/main.rs")"
[ "$(cat "$R/README.md")" = "other work" ] || fail "restore touched README"
[ ! -e "$R/config.toml" ] || fail "restore touched config.toml (only src/main.rs was asked for)"
# The restore itself is a checkpoint, and undoable: restore the file back.
acy timeline --limit 3 | grep -q "restore src/main.rs from #$T2_END" || fail "restore not recorded"
acy restore "$A_END" src/main.rs >/dev/null || fail "restore back"
[ "$(cat "$R/src/main.rs")" = "approach 3" ] || fail "second restore failed"
# Absent-at-checkpoint paths are removed (config.toml existed only in the
# abandoned turn 2); escapes refused.
acy restore "$T2_END" config.toml >/dev/null || fail "restore config.toml from turn 2"
[ "$(cat "$R/config.toml")" = "cfg=2" ] || fail "config.toml content"
acy restore "$T1_END" config.toml | grep -q "removed" || fail "absent path not reported removed"
[ ! -e "$R/config.toml" ] || fail "absent path not removed"
acy restore "$T1_END" ../escape >/dev/null 2>&1 && fail "escape accepted"
# Errors don't wedge the daemon.
acy status | grep -q "state:         ready" || fail "daemon not ready after restore errors"

# ---- 4b. kill -9 the daemon mid-session: metadata is intact afterwards.
hook user-prompt "{\"session_id\":\"$SB\",\"prompt\":\"B turn one\"}"
hook pre-tool "{\"session_id\":\"$SB\",\"tool_name\":\"Edit\",\"tool_use_id\":\"b1\"}"
settle 0.6
kill -9 "$(daemon_pid)" 2>/dev/null || fail "no daemon pid to kill"
sleep 0.5
acy turns --session "$SB" | grep -q 't1 .*"B turn one"' || fail "turn lost across crash"
SHOW2="$(acy show "$T2_END" 2>&1)"
echo "$SHOW2" | grep -q "turn:        2" || fail "checkpoint attribution lost across crash: T2_END=$T2_END: $SHOW2 / $(acy timeline --limit 30 2>&1)"
acy sessions | grep -q "sess-B-0" || fail "session B lost across crash"

pass "timeline: turn linkage both ways, session brief < 1KB with abandoned branches, single-file restore, restart + crash durability"

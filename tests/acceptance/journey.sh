#!/usr/bin/env bash
# The Maya journey from plans/01-rewind.md, scripted end to end:
# init -> risky change (edit + bash side effects) -> diff -> rewind ->
# byte-identical restore incl. gitignored state -> cross-session persistence.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

setup_repo
acy init >/dev/null || fail "init"
acy session-start sess-A --host acceptance || fail "session-start"

# Pre-boundary checkpoint (exact, --wait).
acy checkpoint --wait --kind pre --session-id sess-A -m "before migration" >/dev/null \
  || fail "pre checkpoint"

# The risky change: edit, delete a gitignored secret, generate an artifact.
printf 'MIGRATED\n' > "$R/src/main.rs"
rm "$R/.env"
head -c 8192 /dev/zero > "$R/generated.bin"
settle 0.4
acy checkpoint --wait --kind post --session-id sess-A --tool-name Bash >/dev/null \
  || fail "post checkpoint"

# Blast radius names exactly the changed paths.
DIFF="$(acy diff)"
echo "$DIFF" | grep -q "^D .env" || fail "diff missing .env removal: $DIFF"
echo "$DIFF" | grep -q "^A generated.bin" || fail "diff missing artifact: $DIFF"
echo "$DIFF" | grep -q "^M src/main.rs" || fail "diff missing edit: $DIFF"

# Rewind to the session start; the tree comes back exactly, gitignored
# state included.
acy rewind --session-start sess-A --yes >/dev/null || fail "rewind"
[ "$(cat "$R/src/main.rs")" = "ORIGINAL" ] || fail "edit not reverted"
[ "$(cat "$R/.env")" = "SECRET=1" ] || fail "gitignored .env not restored"
[ ! -e "$R/generated.bin" ] || fail "artifact not removed"

# rewind --last targets the newest REAL checkpoint (post = MIGRATED state),
# not the bookkeeping rows the rewind itself created.
acy rewind --last --yes >/dev/null || fail "rewind --last"
[ "$(cat "$R/src/main.rs")" = "MIGRATED" ] || fail "--last picked a bookkeeping row"

# Publish everything; nothing left unpublished.
acy commit >/dev/null || fail "commit"
STATUS="$(acy status)"
if ! echo "$STATUS" | grep -q "unpublished:   0"; then
  echo "--- status as seen by the check:" >&2
  echo "$STATUS" >&2
  echo "--- rows:" >&2
  sqlite3 "$STORES"/*/index.db \
    "SELECT id, kind, published FROM checkpoints ORDER BY id;" >&2 || true
  fail "unpublished after commit"
fi

# Cross-session persistence: stop the daemon, autospawn a fresh one, and the
# timeline (with session attribution) survives.
acy stop >/dev/null || fail "stop"
sleep 0.5
TL="$(acy timeline --session sess-A)"
echo "$TL" | grep -q "post" || fail "timeline lost after restart: $TL"

# Hook contract: no daemon -> quiet no-op with exit 2, and never a spawn.
acy stop >/dev/null 2>&1 || true
sleep 0.5
set +e
acy --hook checkpoint --kind post >/dev/null 2>&1
CODE=$?
set -e
[ "$CODE" -eq 2 ] || fail "hook exit was $CODE, want 2"
pgrep -f "__daemon $R" >/dev/null 2>&1 && fail "hook spawned a daemon"

pass "journey: checkpoints, diff, rewind selectors, persistence, hook contract"

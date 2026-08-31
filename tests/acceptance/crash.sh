#!/usr/bin/env bash
# Crash matrix: kill -9 the daemon mid-session, verify the store reopens and
# history survives; then simulate a crash mid-rewind-swap and verify the
# journal recovery makes the repo whole before the daemon serves again.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

setup_repo
acy init >/dev/null || fail "init"
printf 'ONE\n' > "$R/src/main.rs"
sleep 0.3
acy checkpoint --wait --kind post >/dev/null || fail "checkpoint one"

# --- kill -9 while captures may be in flight ------------------------------
printf 'TWO\n' > "$R/src/main.rs"
acy checkpoint --kind post >/dev/null   # queued, racing the kill
PID="$(daemon_pid)"
[ -n "$PID" ] || fail "no daemon pid"
kill -9 "$PID"
sleep 0.3

# The next interactive verb autospawns a fresh daemon over the same store
# (stale socket + pidfile included) and history is intact.
TL="$(acy timeline)" || fail "timeline after kill -9"
echo "$TL" | grep -q "post" || fail "history lost after kill -9: $TL"
acy checkpoint --wait --kind post >/dev/null || fail "checkpoint after kill -9"

# --- crash mid-swap: journal-driven recovery ------------------------------
# The atomic-exchange platforms (macOS/Linux) always leave the repo whole;
# a crash there leaves the journal plus a stray tmp tree. The repo-missing
# two-step variant is unit-tested in acyclic-engine (recover()).
acy stop >/dev/null
sleep 0.5
JOURNAL="$(ls -d "$STORES"/*/)"rewind-journal.json
TMP_TREE="$WORK/.repo.acyclic-tmp-crash"

mkdir -p "$TMP_TREE"
printf 'partial\n' > "$TMP_TREE/leftover.txt"
python3 - "$JOURNAL" "$R" "$TMP_TREE" <<'PY'
import json, sys
journal, repo, tmp = sys.argv[1:4]
with open(journal, "w") as f:
    json.dump({
        "target_generation": "00" * 32,
        "repo_root": repo,
        "tmp": tmp,
        "phase": "Swapping",
    }, f)
PY

# Any daemon start runs recovery before opening the store.
acy status >/dev/null || fail "daemon start with pending journal"
[ -f "$R/src/main.rs" ] || fail "repo contents missing after recovery"
[ ! -e "$JOURNAL" ] || fail "journal not cleared after recovery"
[ ! -e "$TMP_TREE" ] || fail "tmp tree left behind after recovery"

# And the store still checkpoints.
printf 'AFTER-RECOVERY\n' > "$R/src/main.rs"
sleep 0.3
acy checkpoint --wait --kind post >/dev/null || fail "checkpoint after recovery"

pass "crash: kill -9 survival + mid-swap journal recovery"

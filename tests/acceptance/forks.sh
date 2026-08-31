#!/usr/bin/env bash
# M-series fork acceptance (spec: plans/spec-forks.md): the mount-dependent
# behavior — N simultaneous fork mounts, isolation, promote journey,
# conflict, evaporation, crash sweep. Skips (exit 0) when the native mount
# layer is unavailable unless ACYCLIC_FORKS_REQUIRED=1.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

skip() {
  if [ "${ACYCLIC_FORKS_REQUIRED:-0}" = "1" ]; then
    fail "$*"
  fi
  echo "SKIP($(basename "$0")): $*"
  exit 0
}

# Environment hygiene: a kill -9'd daemon anywhere orphans its go-nfsv4
# helper, and orphaned helpers wedge FUSE-T's tiny shared NFS port pool for
# every future mount on the host. Reap acyclic helpers + mounts up front.
if [ "$(uname -s)" = "Darwin" ]; then
  pkill -f 'go-nfsv4.*acyclic-fs' 2>/dev/null || true
  sleep 0.5
  mount | awk '/fuse-t:\/acyclic-fs/{print $3}' | while read -r M; do
    umount -f "$M" 2>/dev/null || true
  done
fi

setup_repo
printf 'MAINLINE\n' > "$R/src/app.txt"
acy init >/dev/null || fail "init"
FORKS_ROOT="$(dirname "$R")/.$(basename "$R").forks"

# --- M1: three simultaneous forks, fast ------------------------------------
START=$(python3 -c 'import time; print(time.time())')
if ! FORK_OUT="$(acy fork -n 3 2>&1)"; then
  echo "$FORK_OUT" | grep -qi 'mount' && skip "native mounts unavailable: $FORK_OUT"
  fail "fork -n 3: $FORK_OUT"
fi
ELAPSED="$(python3 -c "import time; print(time.time() - $START)")"
IDS=($(echo "$FORK_OUT" | awk '/^fork /{print $2}'))
[ "${#IDS[@]}" -eq 3 ] || fail "expected 3 forks, got ${#IDS[@]}: $FORK_OUT"
python3 -c "import sys; sys.exit(0 if float('$ELAPSED') < 5.0 else 1)" \
  || fail "3 forks took ${ELAPSED}s (>5s budget incl. base commits)"
# Routed design: every fork is a subdirectory of ONE native session.
MOUNTS="$(mount | grep -c "$FORKS_ROOT" || true)"
[ "$MOUNTS" -eq 1 ] || fail "mount table shows $MOUNTS fork mounts, want exactly 1 (routed)"

MNT="$FORKS_ROOT/mnt"
A="$MNT/${IDS[0]}"
B="$MNT/${IDS[1]}"
C="$MNT/${IDS[2]}"

# --- M2: divergent writes, isolated ---------------------------------------
printf 'FORK-A\n' > "$A/src/app.txt" || fail "write into fork A"
printf 'FORK-B\n' > "$B/src/app.txt" || fail "write into fork B"
printf 'extra\n' > "$C/only-in-c.txt" || fail "write into fork C"
[ "$(cat "$A/src/app.txt")" = "FORK-A" ] || fail "A readback"
[ "$(cat "$B/src/app.txt")" = "FORK-B" ] || fail "B readback"
[ "$(cat "$R/src/app.txt")" = "MAINLINE" ] || fail "I1: mainline saw fork write"
[ ! -e "$A/only-in-c.txt" ] || fail "I2: fork A saw fork C's file"
[ "$(cat "$C/src/app.txt")" = "MAINLINE" ] || fail "I2: fork C saw a sibling edit"

# --- M3: mainline stays usable with forks live ----------------------------
printf 'MAINLINE-NOTE\n' > "$R/note.txt"
sleep 0.4
acy checkpoint --wait --kind post >/dev/null || fail "I3: mainline checkpoint"
rm "$R/note.txt"
sleep 0.4
acy checkpoint --wait --kind post >/dev/null || fail "I3: second checkpoint"

# --- M5 first (so M4's promote still applies cleanly): conflict path ------
# The two checkpoints above moved the mainline past every fork's base, so
# promoting C now must conflict legibly and touch nothing.
if OUT="$(acy promote "${IDS[2]}" 2>&1)"; then
  fail "M5: promote after mainline moved should conflict: $OUT"
fi
echo "$OUT" | grep -q "moved past the fork's base" || fail "M5: conflict not legible: $OUT"
[ "$(cat "$R/src/app.txt")" = "MAINLINE" ] || fail "M5: conflict touched the tree"

# Restore an unmoved mainline for A and B by re-forking from current state.
acy fork-drop "${IDS[0]}" >/dev/null || fail "drop stale A"
acy fork-drop "${IDS[1]}" >/dev/null || fail "drop stale B"
FORK_OUT="$(acy fork -n 2)" || fail "re-fork"
IDS=($(echo "$FORK_OUT" | awk '/^fork /{print $2}'))
A="$MNT/${IDS[0]}"; B="$MNT/${IDS[1]}"
printf 'WINNER\n' > "$A/src/app.txt" || fail "write winner"
printf 'LOSER\n' > "$B/src/app.txt" || fail "write loser"

# --- M4: promote journey --------------------------------------------------
acy promote "${IDS[0]}" >/dev/null || fail "M4: promote"
[ "$(cat "$R/src/app.txt")" = "WINNER" ] || fail "M4: promoted content missing"
[ "$(cat "$R/.env")" = "SECRET=1" ] || fail "M4: gitignored state lost"
acy timeline | grep -q "promote fork ${IDS[0]}" || fail "M4/I6: no promote row"
[ ! -d "$A" ] || fail "M4: promoted workspace not removed"

# --- M6: evaporation + crash sweep ---------------------------------------
acy fork-drop "${IDS[1]}" >/dev/null || fail "M6: fork-drop"
[ ! -d "$B" ] || fail "M6: dropped workspace remains"
[ "$(mount | grep -c "$FORKS_ROOT" || true)" -eq 0 ] || fail "M6: mounts remain"

FORK_OUT="$(acy fork -n 1)" || fail "M6: fork for crash test"
PID="$(daemon_pid)"
kill -9 "$PID"
sleep 0.5
acy status >/dev/null || fail "M6: restart after kill -9"
[ "$(ls "$FORKS_ROOT" 2>/dev/null | wc -l | tr -d ' ')" = "0" ] \
  || fail "M6: stale fork workspaces not swept"
acy forks | grep -q "no live forks" || fail "M6: forks list not reset"

# --- M7: stop cleans up ---------------------------------------------------
acy fork -n 1 >/dev/null || fail "M7: fork"
acy stop >/dev/null || fail "M7: stop"
sleep 0.5
[ "$(mount | grep -c "$FORKS_ROOT" || true)" -eq 0 ] || fail "M7: mount survived stop"

pass "forks: 3-way mounts, isolation, promote, conflict, evaporation, sweep"

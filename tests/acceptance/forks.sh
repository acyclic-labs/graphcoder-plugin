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


# A removed route is DEAD even while the kernel's entry cache still shows
# its name (FSKit caches positives until it decides otherwise): dead means
# no content is served and no fork is listed. The phantom name is a
# documented cosmetic caveat (spec I5).
route_dead() {
  [ -z "$(ls "$1" 2>/dev/null)" ] || return 1
  acy forks | grep -q "$2" && return 1
  return 0
}

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
# A durable checkpoint PUBLISHES the change: the mainline has now truly
# moved past every fork's base (spec: movement = published generation).
printf 'MAINLINE-NOTE-2\n' > "$R/note.txt"
sleep 0.4
acy checkpoint --wait --durable >/dev/null || fail "I3: durable checkpoint"

# --- M5 first (so M4's promote still applies cleanly): merge semantics ----
# The published movement above (note.txt) means the mainline moved past
# every fork's base. Fork C only added only-in-c.txt: disjoint from the
# mainline's change, so promote must REPLAY it in place, no swap, and touch
# nothing else.
OUT="$(acy promote "${IDS[2]}" 2>&1)" || fail "M5: disjoint promote should replay: $OUT"
echo "$OUT" | grep -q "promoted by replay: 1 path" || fail "M5: expected a 1-path replay: $OUT"
[ "$(cat "$R/only-in-c.txt")" = "extra" ] || fail "M5: replayed file missing"
[ "$(cat "$R/note.txt")" = "MAINLINE-NOTE-2" ] || fail "M5: replay clobbered the mainline's own change"
[ "$(cat "$R/src/app.txt")" = "MAINLINE" ] || fail "M5: replay touched an unrelated path"
route_dead "$C" "${IDS[2]}" || fail "M5: replayed fork still serves"

# --- M5b: a same-line overlap conflicts legibly, touches nothing, and
# rebases the fork with markers (merge.sh covers the full contract) ---------
# Fork D edits note.txt; the mainline edits note.txt again and publishes.
D_OUT="$(acy fork -n 1)" || fail "M5b: fork D"
D_ID="$(echo "$D_OUT" | awk '/^fork /{print $2; exit}')"; D="$MNT/$D_ID"
printf 'FORK-D-NOTE\n' > "$D/note.txt" || fail "M5b: write into fork D"
printf 'MAINLINE-NOTE-3\n' > "$R/note.txt"
sleep 0.4
acy checkpoint --wait --durable >/dev/null || fail "M5b: durable checkpoint"
if OUT="$(acy promote "$D_ID" 2>&1)"; then
  fail "M5b: overlapping promote should conflict: $OUT"
fi
echo "$OUT" | grep -q "note.txt: 1 conflicting hunk(s)" || fail "M5b: conflict not legible: $OUT"
[ "$(cat "$R/note.txt")" = "MAINLINE-NOTE-3" ] || fail "M5b: conflict touched the tree"
grep -q '^<<<<<<< fork ' "$D/note.txt" || fail "M5b: markers must be written into the fork: $(cat "$D/note.txt")"
acy fork-drop "$D_ID" >/dev/null || fail "M5b: drop D"

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
TL="$(acy timeline)"
echo "$TL" | grep -q "promote fork ${IDS[0]}" || fail "M4/I6: no promote row: $TL"
route_dead "$A" "${IDS[0]}" || fail "M4: promoted fork still serves"

# --- M6: evaporation + crash sweep ---------------------------------------
acy fork-drop "${IDS[1]}" >/dev/null || fail "M6: fork-drop"
route_dead "$B" "${IDS[1]}" || fail "M6: dropped fork still serves"
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

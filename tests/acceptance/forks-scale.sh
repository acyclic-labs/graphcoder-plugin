#!/usr/bin/env bash
# Fork scale measurement (docs/design/03-forks.md, spec-forks.md).
#
# forks.sh proves the CONTRACT at N=3. This script asks the question the
# contract does not answer: does the routed design actually hold as N grows
# to the hundreds? Three claims are on trial, all of them load-bearing for
# the fan-out story (N subagents on N forks):
#
#   C1 (one mount)   — "One kernel mount total, regardless of N; forks are
#                      route inserts" (spec-forks.md). The mount table must
#                      show exactly 1 at every rung.
#   C2 (O(1) store)  — a fork copies nothing, so store growth per fork is a
#                      small fixed overhead, not a function of tree size.
#   C3 (flat cost)   — per-fork creation time must not degrade as the route
#                      table fills. Route insert #256 should cost about what
#                      insert #1 did.
#
# It is a measurement tool first and a pass/fail test second: the rung table
# prints on every run, because the numbers are the point. The budgets only
# catch a blowup, so they are deliberately loose — C3 allows a 4x drift
# before failing, which is superlinear-detection, not benchmarking.
#
# Batching: `fork -n` is capped at 16 (crates/acyclic/src/server.rs:590), so
# N forks are built from ceil(N/16) calls. Each call publishes its own base
# row, so the rungs are NOT a single-base fan-out; the content is identical
# across bases (nothing edits the mainline during the ladder), so isolation
# and cost are measured faithfully, but a true 400-way race off ONE base
# needs that cap raised. Once it is, set ACYCLIC_SCALE_BATCH to measure the
# single-call path with no other change.
#
#   ACYCLIC_SCALE_RUNGS    ladder (default "16 64 256"; add 1024 for the
#                          full run — slow, and the first place fd limits
#                          and the FUSE-T helper are likely to complain)
#   ACYCLIC_SCALE_BATCH    forks per `fork -n` call (default 16 = the cap)
#   ACYCLIC_SCALE_REQUIRED 1 = fail instead of skip when mounts are absent
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

RUNGS="${ACYCLIC_SCALE_RUNGS:-16 64 256}"
BATCH="${ACYCLIC_SCALE_BATCH:-16}"
DEP_DIRS="${ACYCLIC_SCALE_DEP_DIRS:-200}"
DEP_FILES="${ACYCLIC_SCALE_DEP_FILES:-10}"
BLOB_MB="${ACYCLIC_SCALE_BLOB_MB:-8}"
DEP_TOTAL=$((DEP_DIRS * DEP_FILES))
# Budgets. Per-fork store overhead is a head checkout + route entry; growth.sh
# already holds a whole checkpoint to 128 KiB, so a fork gets the same room.
PER_FORK_STORE_CAP=$((128 * 1024))
TIME_SLOP=4

skip() {
  if [ "${ACYCLIC_SCALE_REQUIRED:-0}" = "1" ]; then
    fail "$*"
  fi
  echo "SKIP($(basename "$0")): $*"
  exit 0
}

# Same hygiene as forks.sh: an orphaned go-nfsv4 helper wedges FUSE-T's
# shared NFS port pool for every later mount on the host, and this script
# is the one most likely to have left one behind.
if [ "$(uname -s)" = "Darwin" ]; then
  pkill -f 'go-nfsv4.*acyclic-fs' 2>/dev/null || true
  sleep 0.5
  mount | awk '/fuse-t:\/acyclic-fs/{print $3}' | while read -r M; do
    umount -f "$M" 2>/dev/null || true
  done
fi

now() { python3 -c 'import time; print(time.time())'; }
secs() { python3 -c "print(f'{($2)-($1):.2f}')" "$@"; }
per_fork_ms() { python3 -c "print(int(((($2)-($1))*1000)/$3))" "$@"; }
store_bytes() { du -sk "$STORES"/*/store | cut -f1 | awk '{s+=$1} END {print s*1024}'; }
mount_count() { mount | grep -c "$FORKS_ROOT" || true; }

# A bulky-ish tree, so "O(1) in repo size" is being measured against
# something rather than against an empty directory.
setup_repo
python3 - "$R" "$DEP_DIRS" "$DEP_FILES" <<'PY'
import os, sys
root, dirs, per = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
for d in range(dirs):
    p = os.path.join(root, "deps", "pkg%03d" % d)
    os.makedirs(p, exist_ok=True)
    for f in range(per):
        with open(os.path.join(p, "m%02d.js" % f), "w") as fh:
            fh.write("module.exports = %d;\n" % (d * per + f))
PY
head -c $((BLOB_MB * 1024 * 1024)) /dev/urandom > "$R/blob.bin"
TREE_KB="$(du -sk "$R" | cut -f1)"

acy init >/dev/null || fail "init"
FORKS_ROOT="$(dirname "$R")/.$(basename "$R").forks"
MNT="$FORKS_ROOT/mnt"

STATUS="$(acy status)"
echo "$STATUS" | grep -q 'mounts: *unavailable' \
  && skip "native mounts unavailable: $(echo "$STATUS" | grep 'mounts:')"

acy checkpoint --wait -m baseline >/dev/null || fail "baseline checkpoint"
acy commit >/dev/null || fail "commit baseline"
BASE_STORE="$(store_bytes)"

echo "  tree: ${TREE_KB} KiB, $DEP_TOTAL dep files + ${BLOB_MB} MiB blob; store after baseline: $((BASE_STORE / 1024)) KiB"
echo "  ladder: $RUNGS (batch $BATCH per \`fork -n\`)"

IDS=()
CREATED=0
FIRST_MS=""
LAST_MS=""
REPORT=""

for RUNG in $RUNGS; do
  [ "$RUNG" -gt "$CREATED" ] || fail "rungs must ascend: $RUNG after $CREATED"
  RUNG_FROM="$CREATED"
  CALLS=0
  RUNG_START="$(now)"
  while [ "$CREATED" -lt "$RUNG" ]; do
    N=$((RUNG - CREATED))
    [ "$N" -gt "$BATCH" ] && N="$BATCH"
    if ! OUT="$(acy fork -n "$N" 2>&1)"; then
      # The two failures worth naming: the shipped cap, and running out of
      # descriptors/routes partway up the ladder.
      echo "$OUT" | grep -q '1\.\.=16' \
        && fail "batch $BATCH exceeds the shipped cap (server.rs: fork count must be 1..=16): $OUT"
      fail "fork -n $N at ${CREATED} live forks: $OUT (if this is EMFILE, raise ulimit -n)"
    fi
    NEW=($(echo "$OUT" | awk '/^fork /{print $2}'))
    [ "${#NEW[@]}" -eq "$N" ] || fail "asked for $N forks, got ${#NEW[@]}: $OUT"
    # Copy-mode forks cost a full tree copy each; a scale ladder in copy mode
    # measures nothing about the routed design and would take all day.
    echo "$OUT" | grep -q '(copy)' && skip "forks fell back to copy mode; scale applies to the routed path"
    IDS+=("${NEW[@]}")
    CREATED=$((CREATED + N))
    CALLS=$((CALLS + 1))
  done
  RUNG_END="$(now)"

  MADE=$((CREATED - RUNG_FROM))
  MS="$(per_fork_ms "$RUNG_START" "$RUNG_END" "$MADE")"
  [ -z "$FIRST_MS" ] && FIRST_MS="$MS"
  LAST_MS="$MS"
  GROWTH=$(( $(store_bytes) - BASE_STORE ))
  PER_FORK=$((GROWTH / CREATED))
  MOUNTS="$(mount_count)"
  REPORT="$REPORT  N=$(printf '%5d' "$CREATED")  +${MADE} in $(secs "$RUNG_START" "$RUNG_END")s ($CALLS calls)  ${MS} ms/fork  store +$((GROWTH / 1024)) KiB ($((PER_FORK / 1024)) KiB/fork)  mounts=$MOUNTS
"

  # C1, checked at every rung rather than only at the top: a design that
  # silently starts adding kernel mounts should fail where it started.
  [ "$MOUNTS" -eq 1 ] || fail "C1: $MOUNTS mounts at N=$CREATED, want exactly 1 (routed)"
done

printf '%s' "$REPORT"

# --- C2: store growth is per-fork overhead, not a copy of the tree --------
GROWTH=$(( $(store_bytes) - BASE_STORE ))
PER_FORK=$((GROWTH / CREATED))
[ "$PER_FORK" -lt "$PER_FORK_STORE_CAP" ] \
  || fail "C2: $PER_FORK bytes per fork exceeds $PER_FORK_STORE_CAP"
[ "$GROWTH" -lt $((TREE_KB * 1024)) ] \
  || fail "C2: $CREATED forks grew the store by $GROWTH bytes, more than one copy of the tree ($((TREE_KB * 1024)))"

# --- C3: the route table does not get slower as it fills ------------------
python3 -c "import sys; sys.exit(0 if $LAST_MS <= max($FIRST_MS, 1) * $TIME_SLOP else 1)" \
  || fail "C3: ${LAST_MS} ms/fork at N=$CREATED vs ${FIRST_MS} ms/fork at the first rung (>${TIME_SLOP}x)"

# --- Spot checks: a fork at scale is still a real, isolated tree -----------
# Cheap to create is worthless if the 256th fork serves nothing. Sample the
# ends and the middle rather than all N (which would dominate the runtime).
SAMPLE=("${IDS[0]}" "${IDS[$((CREATED / 2))]}" "${IDS[$((CREATED - 1))]}")
for ID in "${SAMPLE[@]}"; do
  P="$MNT/$ID"
  [ "$(cat "$P/src/main.rs" 2>/dev/null)" = "ORIGINAL" ] || fail "fork $ID does not serve src/main.rs"
  [ "$(cat "$P/.env" 2>/dev/null)" = "SECRET=1" ] || fail "fork $ID lost gitignored state"
  COUNT="$(find "$P/deps" -type f | wc -l | tr -d ' ')"
  [ "$COUNT" -eq "$DEP_TOTAL" ] || fail "fork $ID serves $COUNT dep files, want $DEP_TOTAL"
  printf 'PROBE-%s\n' "$ID" > "$P/scale-probe.txt" || fail "fork $ID is not writable"
done

FIRST="$MNT/${SAMPLE[0]}"
LAST="$MNT/${SAMPLE[2]}"
[ "$(cat "$FIRST/scale-probe.txt")" = "PROBE-${SAMPLE[0]}" ] || fail "I2: first fork's probe was overwritten"
[ "$(cat "$LAST/scale-probe.txt")" = "PROBE-${SAMPLE[2]}" ] || fail "I2: last fork's probe was overwritten"
[ ! -e "$R/scale-probe.txt" ] || fail "I1: a fork write reached the mainline at N=$CREATED"

# --- Teardown cost: losers have to evaporate as cheaply as they were made --
DROP_START="$(now)"
for ID in "${SAMPLE[@]}"; do
  acy fork-drop "$ID" >/dev/null || fail "fork-drop $ID at N=$CREATED"
done
echo "  drop: 3 forks in $(secs "$DROP_START" "$(now)")s at N=$CREATED"

acy stop >/dev/null || fail "stop"
sleep 1
[ "$(mount_count)" -eq 0 ] || fail "mount survived stop after $CREATED forks"
[ "$(ls "$FORKS_ROOT" 2>/dev/null | wc -l | tr -d ' ')" = "0" ] \
  || fail "stale fork workspaces after $CREATED forks"

pass "forks-scale: $CREATED forks, 1 mount, $((PER_FORK / 1024)) KiB/fork, ${FIRST_MS}->${LAST_MS} ms/fork"

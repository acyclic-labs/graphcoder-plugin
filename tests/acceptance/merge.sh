#!/usr/bin/env bash
# Merge-series acceptance: what `acyclic promote` does when the mainline has
# moved past a fork's base. The claim under test is "independent merges":
# a fork lands onto a moved mainline when its changed paths are disjoint
# from everything that changed since the base, and is refused (naming the
# paths) when they are not. Content inside a file is never merged.
#
# Works in mount mode (macOS NFS loopback, Linux FUSE) and copy mode (no
# mount provider): the merge is judged on generations, not on how the fork
# was realized. Each case starts from a fresh fork set so the cases are
# independent of one another.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

setup_repo
mkdir -p "$R/docs" "$R/lib/util"
printf 'auth v0\n' > "$R/src/auth.js"
printf 'billing v0\n' > "$R/src/billing.js"
printf 'line1\nline2\nline3\n' > "$R/src/shared.js"
printf 'old\n' > "$R/docs/old.md"
printf 'helper\n' > "$R/lib/util/helper.js"
acy init >/dev/null || fail "init"
MODE="$(acy status | awk '/^mounts:/{print ($2=="unavailable") ? "copy" : "mount"}')"
echo "merge.sh: fork mode is $MODE"

fork() { acy fork -n 1 | awk '/^fork /{print $2; exit}'; }
fork_path() { acy forks | awk -v f="$1" '$1==f{print $5}'; }
head_id() { acy timeline | head -1 | awk '{print $1}' | tr -d '#'; }
promote_ok() {
  local out
  out="$(acy promote "$1" 2>&1)" || fail "$2: promote failed: $out"
  echo "$out"
}
promote_refused() {
  local out
  if out="$(acy promote "$1" 2>&1)"; then
    fail "$2: promote should have been refused: $out"
  fi
  echo "$out"
}
tree_unchanged_except() {
  # $1: label, then path=expected pairs for paths that MAY differ from the
  # seeded content ("<absent>" means must not exist); every other seeded
  # path must still hold its v0 content.
  local label="$1"; shift
  local p exp kv
  for p in src/auth.js src/billing.js src/shared.js docs/old.md lib/util/helper.js .env; do
    case "$p" in
      src/auth.js) exp='auth v0' ;; src/billing.js) exp='billing v0' ;;
      src/shared.js) exp=$'line1\nline2\nline3' ;; docs/old.md) exp='old' ;;
      lib/util/helper.js) exp='helper' ;; .env) exp='SECRET=1' ;;
    esac
    for kv in "$@"; do
      [ "${kv%%=*}" = "$p" ] && exp="${kv#*=}"
    done
    if [ "$exp" = "<absent>" ]; then
      [ ! -e "$R/$p" ] || fail "$label: $p should be absent"
    else
      [ "$(cat "$R/$p" 2>/dev/null)" = "$exp" ] || fail "$label: $p is '$(cat "$R/$p" 2>/dev/null)', want '$exp'"
    fi
  done
}

# --- G1: two disjoint forks both land; the second by replay ----------------
A="$(fork)"; B="$(fork)"
printf 'auth v1\n' > "$(fork_path "$A")/src/auth.js"
printf 'billing v1\n' > "$(fork_path "$B")/src/billing.js"
rm "$(fork_path "$B")/docs/old.md"
OUT="$(promote_ok "$A" G1)"
echo "$OUT" | grep -q '^promoted: working tree now at' || fail "G1: first promote should swap: $OUT"
OUT="$(promote_ok "$B" G1)"
echo "$OUT" | grep -q 'promoted by replay: 2 path' || fail "G1: second promote should replay 2 paths: $OUT"
tree_unchanged_except G1 src/auth.js='auth v1' src/billing.js='billing v1' docs/old.md='<absent>'
acy forks | grep -q 'no live forks' || fail "G1: forks should be consumed"

# --- G2: three disjoint forks all land, in any order -------------------------
printf 'old\n' > "$R/docs/old.md"; acy checkpoint -m "G2 base" >/dev/null
A="$(fork)"; B="$(fork)"; C="$(fork)"
printf 'one\n' > "$(fork_path "$A")/one.txt"
printf 'two\n' > "$(fork_path "$B")/two.txt"
printf 'three\n' > "$(fork_path "$C")/lib/util/three.txt"
promote_ok "$C" G2 >/dev/null; promote_ok "$A" G2 >/dev/null; promote_ok "$B" G2 >/dev/null
[ "$(cat "$R/one.txt" "$R/two.txt" "$R/lib/util/three.txt" | tr '\n' ' ')" = "one two three " ] || fail "G2: not all three landed"
tree_unchanged_except G2 src/auth.js='auth v1' src/billing.js='billing v1'

# --- G3: same file on both sides is refused, names the path, touches nothing
A="$(fork)"; B="$(fork)"
printf 'auth A\n' > "$(fork_path "$A")/src/auth.js"
printf 'auth B\n' > "$(fork_path "$B")/src/auth.js"
printf 'bill B\n' > "$(fork_path "$B")/src/billing.js"
promote_ok "$A" G3 >/dev/null
OUT="$(promote_refused "$B" G3)"
echo "$OUT" | grep -q 'both sides changed 1 path(s): src/auth.js' || fail "G3: conflict must name src/auth.js: $OUT"
tree_unchanged_except G3 src/auth.js='auth A' src/billing.js='billing v1'
acy forks | grep -q 'no live forks' || fail "G3: refused fork should be discarded, not left live"

# --- G4: different LINES of the same file are still a conflict (no content merge)
A="$(fork)"; B="$(fork)"
printf 'line1 A\nline2\nline3\n' > "$(fork_path "$A")/src/shared.js"
printf 'line1\nline2\nline3 B\n' > "$(fork_path "$B")/src/shared.js"
promote_ok "$A" G4 >/dev/null
OUT="$(promote_refused "$B" G4)"
echo "$OUT" | grep -q 'src/shared.js' || fail "G4: line-level disjointness must NOT merge: $OUT"
[ "$(cat "$R/src/shared.js")" = $'line1 A\nline2\nline3' ] || fail "G4: tree changed on refusal"

# --- G5: ancestry overlap: one fork removes a directory, another adds inside it
A="$(fork)"; B="$(fork)"
rm -r "$(fork_path "$A")/lib/util"
printf 'x\n' > "$(fork_path "$B")/lib/util/new.js"
promote_ok "$A" G5 >/dev/null
OUT="$(promote_refused "$B" G5)"
echo "$OUT" | grep -q 'lib/util/new.js' || fail "G5: ancestry overlap must be refused naming the inner path: $OUT"
[ ! -e "$R/lib/util" ] || fail "G5: refused replay recreated lib/util"

# --- G6: the reverse ancestry direction: add inside, then delete the parent --
mkdir -p "$R/lib/util"; printf 'helper\n' > "$R/lib/util/helper.js"; acy checkpoint -m "G6 base" >/dev/null
A="$(fork)"; B="$(fork)"
printf 'x\n' > "$(fork_path "$A")/lib/util/new.js"
rm -r "$(fork_path "$B")/lib/util"
promote_ok "$A" G6 >/dev/null
OUT="$(promote_refused "$B" G6)"
echo "$OUT" | grep -q 'lib/util' || fail "G6: deleting a dir another fork added into must be refused: $OUT"
[ "$(cat "$R/lib/util/new.js")" = "x" ] || fail "G6: tree changed on refusal"

# --- G7: a replay carries deletions and nested additions, not just edits ----
A="$(fork)"; B="$(fork)"
printf 'auth v2\n' > "$(fork_path "$A")/src/auth.js"
rm "$(fork_path "$B")/src/billing.js"
mkdir -p "$(fork_path "$B")/docs/deep/er"; printf 'deep\n' > "$(fork_path "$B")/docs/deep/er/file.md"
promote_ok "$A" G7 >/dev/null
OUT="$(promote_ok "$B" G7)"
echo "$OUT" | grep -q 'promoted by replay' || fail "G7: expected replay: $OUT"
[ ! -e "$R/src/billing.js" ] || fail "G7: deletion not replayed"
[ "$(cat "$R/docs/deep/er/file.md")" = "deep" ] || fail "G7: nested addition not replayed"
[ "$(cat "$R/src/auth.js")" = "auth v2" ] || fail "G7: replay clobbered the other fork's landed change"

# --- G8: the mainline moved by a direct edit (not a fork): still disjoint-merges
printf 'billing v0\n' > "$R/src/billing.js"; acy checkpoint -m "G8 base" >/dev/null
A="$(fork)"
printf 'from fork\n' > "$(fork_path "$A")/from-fork.txt"
printf 'edited on mainline\n' > "$R/src/auth.js"
acy checkpoint --wait -m "G8 mainline edit" >/dev/null
OUT="$(promote_ok "$A" G8)"
echo "$OUT" | grep -q 'promoted by replay: 1 path' || fail "G8: expected a 1-path replay: $OUT"
[ "$(cat "$R/from-fork.txt")" = "from fork" ] || fail "G8: fork file missing"
[ "$(cat "$R/src/auth.js")" = "edited on mainline" ] || fail "G8: replay clobbered the mainline edit"

# --- G9: gitignored state survives a replay (it survives a swap already) ----
[ "$(cat "$R/.env")" = "SECRET=1" ] || fail "G9: .env lost across replays"

# --- G10: a replay is undoable: rewind --last returns to the pre-replay tree
A="$(fork)"
printf 'undo me\n' > "$(fork_path "$A")/undo.txt"
printf 'moved\n' > "$R/moved.txt"; acy checkpoint --wait -m "G10 mainline" >/dev/null
promote_ok "$A" G10 >/dev/null
[ "$(cat "$R/undo.txt")" = "undo me" ] || fail "G10: replay missing"
TL="$(acy timeline)"
echo "$TL" | grep -q "replayed 1 paths onto moved mainline" || fail "G10: no replay row in timeline: $TL"
echo "$TL" | grep -q "before promote fork .* (replay)" || fail "G10: no pre-replay safety row: $TL"
echo "$TL" | grep -q "fork .* snapshot" || fail "G10: no fork snapshot row: $TL"
SAFETY="$(echo "$TL" | awk '/before promote fork .* \(replay\)/{print $1; exit}' | tr -d '#')"
acy rewind -y "$SAFETY" >/dev/null || fail "G10: rewind to safety checkpoint"
[ ! -e "$R/undo.txt" ] || fail "G10: rewind did not undo the replay"
[ "$(cat "$R/moved.txt")" = "moved" ] || fail "G10: rewind lost the mainline's own change"

# --- G11: an unmoved mainline still lands by whole-tree swap ----------------
A="$(fork)"
printf 'swap\n' > "$(fork_path "$A")/src/auth.js"
OUT="$(promote_ok "$A" G11)"
echo "$OUT" | grep -q '^promoted: working tree now at' || fail "G11: unmoved mainline should swap, not replay: $OUT"
echo "$OUT" | grep -q 'old tree kept at' || fail "G11: swap should keep the old tree: $OUT"

# --- G12: a fork with no content change lands nothing, moved or not ---------
A="$(fork)"
printf 'moved again\n' > "$R/moved.txt"; acy checkpoint --wait -m "G12 mainline" >/dev/null
OUT="$(promote_ok "$A" G12)"
echo "$OUT" | grep -q 'fork had no changes' || fail "G12: no-op fork should report no changes: $OUT"

pass "merge ($MODE mode): disjoint forks land by replay in any order, same-file and ancestry overlaps are refused by name, no content merge, deletions and nested adds replay, direct mainline edits merge, replay is undoable, unmoved mainline still swaps"

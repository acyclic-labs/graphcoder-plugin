#!/usr/bin/env bash
# Merge-series acceptance: what `acyclic promote` does when the mainline has
# moved past a fork's base. v1 claimed "independent merges" (disjoint paths
# land, anything else is refused). v2 adds content-level merges with
# graphcoder's semantics: a file both sides edited is merged three-way by
# line; same-line edits, modify/delete, and add/add conflicts write diff3
# markers INTO THE FORK (the mainline is untouched, the fork is rebased onto
# the head) and are resolved there and promoted again; binary, oversized,
# kind-changed, and directory-ancestry overlaps are refused outright and
# leave the fork untouched.
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
printf 'l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\n' > "$R/src/nine.js"
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
has_markers() { grep -q '^<<<<<<< ' "$1" && grep -q '^=======$' "$1"; }
fork_live() { acy forks | awk '$1==f{found=1} END{exit !found}' f="$1"; }
tree_unchanged_except() {
  # $1: label, then path=expected pairs for paths that MAY differ from the
  # seeded content ("<absent>" means must not exist); every other seeded
  # path must still hold its v0 content.
  local label="$1"; shift
  local p exp kv
  for p in src/auth.js src/billing.js src/shared.js src/nine.js docs/old.md lib/util/helper.js .env; do
    case "$p" in
      src/auth.js) exp='auth v0' ;; src/billing.js) exp='billing v0' ;;
      src/shared.js) exp=$'line1\nline2\nline3' ;; docs/old.md) exp='old' ;;
      src/nine.js) exp=$'l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9' ;;
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

# --- G3: the same line on both sides: a conflict, written into the fork -----
A="$(fork)"; B="$(fork)"
printf 'auth A\n' > "$(fork_path "$A")/src/auth.js"
printf 'auth B\n' > "$(fork_path "$B")/src/auth.js"
printf 'bill B\n' > "$(fork_path "$B")/src/billing.js"
promote_ok "$A" G3 >/dev/null
OUT="$(promote_refused "$B" G3)"
echo "$OUT" | grep -q '1 file(s) conflict' || fail "G3: conflict must be reported: $OUT"
echo "$OUT" | grep -q 'src/auth.js: 1 conflicting hunk(s)' || fail "G3: conflict must name src/auth.js with a hunk count: $OUT"
tree_unchanged_except G3 src/auth.js='auth A' src/billing.js='billing v1'
fork_live "$B" || fail "G3: a conflicting fork must stay live"
acy forks | grep "$B" | grep -q 'conflict: 1 path(s)' || fail "G3: forks must show the open conflict: $(acy forks)"
has_markers "$(fork_path "$B")/src/auth.js" || fail "G3: fork file must carry markers: $(cat "$(fork_path "$B")/src/auth.js")"
grep -q "^<<<<<<< fork $B\$" "$(fork_path "$B")/src/auth.js" || fail "G3: ours label"
grep -q '^||||||| original$' "$(fork_path "$B")/src/auth.js" || fail "G3: original block"
grep -q '^>>>>>>> mainline$' "$(fork_path "$B")/src/auth.js" || fail "G3: theirs label"
# The rebase brought the fork up to the head: its other edit is intact and
# it now sees the mainline's landed content elsewhere.
[ "$(cat "$(fork_path "$B")/src/billing.js")" = "bill B" ] || fail "G3: rebase lost the fork's own edit"
acy fork-drop "$B" >/dev/null

# --- G4: different LINES of the same file merge by content ------------------
A="$(fork)"; B="$(fork)"
printf 'line1 A\nline2\nline3\n' > "$(fork_path "$A")/src/shared.js"
printf 'line1\nline2\nline3 B\n' > "$(fork_path "$B")/src/shared.js"
promote_ok "$A" G4 >/dev/null
OUT="$(promote_ok "$B" G4)"
echo "$OUT" | grep -q 'promoted by merge: 1 file(s) merged, 1 path(s) written in place' || fail "G4: expected a content merge: $OUT"
[ "$(cat "$R/src/shared.js")" = $'line1 A\nline2\nline3 B' ] || fail "G4: merged content wrong: $(cat "$R/src/shared.js")"
acy forks | grep -q 'no live forks' || fail "G4: merged fork should be consumed"

# --- G5: ancestry: one fork removes a directory, another adds inside it -----
A="$(fork)"; B="$(fork)"
rm -r "$(fork_path "$A")/lib/util"
printf 'x\n' > "$(fork_path "$B")/lib/util/new.js"
promote_ok "$A" G5 >/dev/null
OUT="$(promote_refused "$B" G5)"
echo "$OUT" | grep -q 'cannot be merged' || fail "G5: ancestry overlap must be refused: $OUT"
echo "$OUT" | grep -q 'lib/util/new.js' || fail "G5: refusal must name the inner path: $OUT"
[ ! -e "$R/lib/util" ] || fail "G5: refused merge recreated lib/util"
fork_live "$B" || fail "G5: a refused fork must stay live"
[ "$(cat "$(fork_path "$B")/lib/util/new.js")" = "x" ] || fail "G5: refusal must leave the fork untouched"
acy fork-drop "$B" >/dev/null

# --- G6: the reverse ancestry direction: add inside, then delete the parent --
mkdir -p "$R/lib/util"; printf 'helper\n' > "$R/lib/util/helper.js"; acy checkpoint -m "G6 base" >/dev/null
A="$(fork)"; B="$(fork)"
printf 'x\n' > "$(fork_path "$A")/lib/util/new.js"
rm -r "$(fork_path "$B")/lib/util"
promote_ok "$A" G6 >/dev/null
OUT="$(promote_refused "$B" G6)"
echo "$OUT" | grep -q 'lib/util: deleted on one side' || fail "G6: deleting a dir another fork added into must be refused: $OUT"
[ "$(cat "$R/lib/util/new.js")" = "x" ] || fail "G6: tree changed on refusal"
acy fork-drop "$B" >/dev/null

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

# --- G8: the mainline moved by a direct edit (not a fork): still merges -----
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

# --- G10: a replay is undoable: rewind to the pre-merge safety row ----------
A="$(fork)"
printf 'undo me\n' > "$(fork_path "$A")/undo.txt"
printf 'moved\n' > "$R/moved.txt"; acy checkpoint --wait -m "G10 mainline" >/dev/null
promote_ok "$A" G10 >/dev/null
[ "$(cat "$R/undo.txt")" = "undo me" ] || fail "G10: replay missing"
TL="$(acy timeline)"
echo "$TL" | grep -q "merged 0 file(s), replayed 1 path(s) onto moved mainline" || fail "G10: no landed row in timeline: $TL"
echo "$TL" | grep -q "before promote fork .* (merge)" || fail "G10: no pre-merge safety row: $TL"
echo "$TL" | grep -q "fork .* merge (0 merged, 1 replayed)" || fail "G10: no merge generation row: $TL"
SAFETY="$(echo "$TL" | awk '/before promote fork .* \(merge\)/{print $1; exit}' | tr -d '#')"
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

# --- G13/G14/G15/G16: conflict, unresolved re-promote, resolve, moved again --
printf 'line1\nline2\nline3\n' > "$R/src/shared.js"; acy checkpoint -m "G13 base" >/dev/null
A="$(fork)"; B="$(fork)"
printf 'line1\nline2 A\nline3\n' > "$(fork_path "$A")/src/shared.js"
printf 'line1\nline2 B\nline3\n' > "$(fork_path "$B")/src/shared.js"
printf 'from B\n' > "$(fork_path "$B")/b-only.txt"
promote_ok "$A" G13 >/dev/null
OUT="$(promote_refused "$B" G13)"
echo "$OUT" | grep -q 'src/shared.js: 1 conflicting hunk(s)' || fail "G13: expected a 1-hunk conflict: $OUT"
[ "$(cat "$R/src/shared.js")" = $'line1\nline2 A\nline3' ] || fail "G13: mainline touched on conflict"
[ ! -e "$R/b-only.txt" ] || fail "G13: nothing may land on a conflict (all-or-nothing)"
BF="$(fork_path "$B")/src/shared.js"
has_markers "$BF" || fail "G13: fork must hold markers"
[ "$(cat "$BF")" = "$(printf 'line1\n<<<<<<< fork %s\nline2 B\n||||||| original\nline2\n=======\nline2 A\n>>>>>>> mainline\nline3' "$B")" ] || fail "G13: diff3 block wrong: $(cat "$BF")"
[ "$(cat "$(fork_path "$B")/b-only.txt")" = "from B" ] || fail "G13: rebase lost fork-only file"
# G15: promoting again with the markers still there is refused, nothing moves.
OUT="$(promote_refused "$B" G15)"
echo "$OUT" | grep -q 'unresolved conflict markers in: src/shared.js' || fail "G15: unresolved markers must be refused by name: $OUT"
fork_live "$B" || fail "G15: fork must survive an unresolved re-promote"
[ "$(cat "$R/src/shared.js")" = $'line1\nline2 A\nline3' ] || fail "G15: mainline touched"
# G14: resolve in the fork, promote again: lands.
printf 'line1\nline2 AB\nline3\n' > "$BF"
OUT="$(promote_ok "$B" G14)"
[ "$(cat "$R/src/shared.js")" = $'line1\nline2 AB\nline3' ] || fail "G14: resolution did not land: $(cat "$R/src/shared.js")"
[ "$(cat "$R/b-only.txt")" = "from B" ] || fail "G14: fork-only file did not land with the resolution"
acy forks | grep -q 'no live forks' || fail "G14: resolved fork should be consumed"
# G16: a conflict, then the mainline moves AGAIN before the resolution lands.
A="$(fork)"; B="$(fork)"
printf 'line1\nline2 X\nline3\n' > "$(fork_path "$A")/src/shared.js"
printf 'line1\nline2 Y\nline3\n' > "$(fork_path "$B")/src/shared.js"
promote_ok "$A" G16 >/dev/null
promote_refused "$B" G16 >/dev/null
printf 'line1\nline2 XY\nline3\n' > "$(fork_path "$B")/src/shared.js"
printf 'moved once more\n' > "$R/moved.txt"; acy checkpoint --wait -m "G16 mainline" >/dev/null
OUT="$(promote_ok "$B" G16)"
echo "$OUT" | grep -q 'promoted by replay: 1 path' || fail "G16: resolution onto a re-moved mainline should replay: $OUT"
[ "$(cat "$R/src/shared.js")" = $'line1\nline2 XY\nline3' ] || fail "G16: resolution lost"
[ "$(cat "$R/moved.txt")" = "moved once more" ] || fail "G16: second mainline move lost"
# ...and if the mainline moves onto the SAME lines again, it conflicts again.
A="$(fork)"; B="$(fork)"
printf 'line1\nline2 P\nline3\n' > "$(fork_path "$A")/src/shared.js"
printf 'line1\nline2 Q\nline3\n' > "$(fork_path "$B")/src/shared.js"
promote_ok "$A" G16 >/dev/null
promote_refused "$B" G16 >/dev/null
printf 'line1\nline2 PQ\nline3\n' > "$(fork_path "$B")/src/shared.js"
printf 'line1\nline2 P2\nline3\n' > "$R/src/shared.js"; acy checkpoint --wait -m "G16 mainline same lines" >/dev/null
OUT="$(promote_refused "$B" G16)"
echo "$OUT" | grep -q 'src/shared.js: 1 conflicting hunk(s)' || fail "G16: re-moved same lines must conflict again: $OUT"
acy fork-drop "$B" >/dev/null

# --- G17: identical change on both sides lands as nothing / a replay --------
printf 'line1\nline2\nline3\n' > "$R/src/shared.js"; acy checkpoint -m "G17 base" >/dev/null
A="$(fork)"; B="$(fork)"; C="$(fork)"
printf 'same\n' > "$(fork_path "$A")/src/auth.js"
printf 'same\n' > "$(fork_path "$B")/src/auth.js"
printf 'same\n' > "$(fork_path "$C")/src/auth.js"; printf 'extra\n' > "$(fork_path "$C")/extra.txt"
promote_ok "$A" G17 >/dev/null
OUT="$(promote_ok "$B" G17)"
echo "$OUT" | grep -q 'fork had no changes' || fail "G17: identical change alone lands nothing: $OUT"
OUT="$(promote_ok "$C" G17)"
echo "$OUT" | grep -q 'promoted by replay: 1 path' || fail "G17: identical change plus a new file replays only the file: $OUT"
[ "$(cat "$R/src/auth.js")" = "same" ] && [ "$(cat "$R/extra.txt")" = "extra" ] || fail "G17: content"

# --- G18: modify/delete: a labelled conflict; deleting in the fork resolves --
A="$(fork)"; B="$(fork)"
rm "$(fork_path "$A")/src/billing.js"
printf 'billing edited\n' > "$(fork_path "$B")/src/billing.js"
promote_ok "$A" G18 >/dev/null
OUT="$(promote_refused "$B" G18)"
echo "$OUT" | grep -q 'src/billing.js: mainline deleted, fork modified' || fail "G18: modify/delete must be labelled: $OUT"
BF="$(fork_path "$B")/src/billing.js"
grep -q "^<<<<<<< fork $B (modified)\$" "$BF" || fail "G18: (modified) label missing: $(cat "$BF")"
grep -q '^>>>>>>> mainline (deleted)$' "$BF" || fail "G18: (deleted) label missing: $(cat "$BF")"
[ ! -e "$R/src/billing.js" ] || fail "G18: mainline touched"
rm "$BF"
OUT="$(promote_ok "$B" G18)"
[ ! -e "$R/src/billing.js" ] || fail "G18: deletion chosen in the fork must hold on the mainline"
acy forks | grep -q 'no live forks' || fail "G18: fork should be consumed"
# and the other direction: the fork deletes, the mainline modifies.
printf 'billing v0\n' > "$R/src/billing.js"; acy checkpoint -m "G18b base" >/dev/null
A="$(fork)"; B="$(fork)"
printf 'billing head\n' > "$(fork_path "$A")/src/billing.js"
rm "$(fork_path "$B")/src/billing.js"
promote_ok "$A" G18b >/dev/null
OUT="$(promote_refused "$B" G18b)"
echo "$OUT" | grep -q 'src/billing.js: fork deleted, mainline modified' || fail "G18b: label: $OUT"
grep -q "^<<<<<<< fork $B (deleted)\$" "$(fork_path "$B")/src/billing.js" || fail "G18b: (deleted) label on ours"
printf 'billing head\n' > "$(fork_path "$B")/src/billing.js"   # keep the mainline's version
promote_ok "$B" G18b >/dev/null
[ "$(cat "$R/src/billing.js")" = "billing head" ] || fail "G18b: content"

# --- G19: add/add with different content conflicts with an empty original --
A="$(fork)"; B="$(fork)"
printf 'A made it\n' > "$(fork_path "$A")/new.txt"
printf 'B made it\n' > "$(fork_path "$B")/new.txt"
promote_ok "$A" G19 >/dev/null
OUT="$(promote_refused "$B" G19)"
echo "$OUT" | grep -q 'new.txt: 1 conflicting hunk(s)' || fail "G19: add/add must conflict: $OUT"
[ "$(cat "$R/new.txt")" = "A made it" ] || fail "G19: mainline touched"
[ "$(cat "$(fork_path "$B")/new.txt")" = "$(printf '<<<<<<< fork %s\nB made it\n||||||| original\n=======\nA made it\n>>>>>>> mainline' "$B")" ] || fail "G19: empty original block expected: $(cat "$(fork_path "$B")/new.txt")"
acy fork-drop "$B" >/dev/null

# --- G20: both sides add different files under one NEW directory: lands -----
A="$(fork)"; B="$(fork)"
mkdir -p "$(fork_path "$A")/pkg/new"; printf 'a\n' > "$(fork_path "$A")/pkg/new/a.txt"
mkdir -p "$(fork_path "$B")/pkg/new"; printf 'b\n' > "$(fork_path "$B")/pkg/new/b.txt"
promote_ok "$A" G20 >/dev/null
OUT="$(promote_ok "$B" G20)"
echo "$OUT" | grep -q 'promoted by replay: 1 path' || fail "G20: independent additions under one new dir must land: $OUT"
[ "$(cat "$R/pkg/new/a.txt")" = "a" ] && [ "$(cat "$R/pkg/new/b.txt")" = "b" ] || fail "G20: both files must be present"

# --- G21: binary on one side is refused; the fork is untouched --------------
printf 'text\n' > "$R/blob.bin"; acy checkpoint -m "G21 base" >/dev/null
A="$(fork)"; B="$(fork)"
printf 'A text\n' > "$(fork_path "$A")/blob.bin"
printf 'B\000bin\n' > "$(fork_path "$B")/blob.bin"
promote_ok "$A" G21 >/dev/null
OUT="$(promote_refused "$B" G21)"
echo "$OUT" | grep -q 'blob.bin: binary' || fail "G21: binary must be refused by name: $OUT"
[ "$(cat "$R/blob.bin")" = "A text" ] || fail "G21: mainline touched"
fork_live "$B" || fail "G21: refused fork must stay live"
cmp -s "$(fork_path "$B")/blob.bin" <(printf 'B\000bin\n') || fail "G21: refusal must leave the fork's bytes alone"
acy fork-drop "$B" >/dev/null

# --- G22: three forks edit one file in disjoint regions, promoted in turn ---
A="$(fork)"; B="$(fork)"; C="$(fork)"
sed -e 's/^l1$/l1 A/' "$R/src/nine.js" > "$(fork_path "$A")/src/nine.js"
sed -e 's/^l5$/l5 B/' "$R/src/nine.js" > "$(fork_path "$B")/src/nine.js"
sed -e 's/^l9$/l9 C/' "$R/src/nine.js" > "$(fork_path "$C")/src/nine.js"
promote_ok "$A" G22 >/dev/null
OUT="$(promote_ok "$B" G22)"; echo "$OUT" | grep -q 'promoted by merge: 1 file' || fail "G22: B should merge: $OUT"
OUT="$(promote_ok "$C" G22)"; echo "$OUT" | grep -q 'promoted by merge: 1 file' || fail "G22: C should merge against the merged result: $OUT"
[ "$(cat "$R/src/nine.js")" = $'l1 A\nl2\nl3\nl4\nl5 B\nl6\nl7\nl8\nl9 C' ] || fail "G22: three-way sequence wrong: $(cat "$R/src/nine.js")"

# --- G23: a merge and disjoint paths in one promote report both counts ------
A="$(fork)"; B="$(fork)"
sed -e 's/^l2$/l2 A/' "$R/src/nine.js" > "$(fork_path "$A")/src/nine.js"
sed -e 's/^l8$/l8 B/' "$R/src/nine.js" > "$(fork_path "$B")/src/nine.js"
printf 'p\n' > "$(fork_path "$B")/p.txt"; printf 'q\n' > "$(fork_path "$B")/docs/q.md"
promote_ok "$A" G23 >/dev/null
OUT="$(promote_ok "$B" G23)"
echo "$OUT" | grep -q 'promoted by merge: 1 file(s) merged, 3 path(s) written in place' || fail "G23: counts: $OUT"
[ "$(cat "$R/p.txt")" = "p" ] && [ "$(cat "$R/docs/q.md")" = "q" ] || fail "G23: disjoint paths missing"
grep -q '^l2 A$' "$R/src/nine.js" && grep -q '^l8 B$' "$R/src/nine.js" || fail "G23: merged file wrong"

# --- G24: a landed content merge is undoable via its safety row -------------
A="$(fork)"
sed -e 's/^l3$/l3 fork/' "$R/src/nine.js" > "$(fork_path "$A")/src/nine.js"
sed -i '' -e 's/^l7$/l7 main/' "$R/src/nine.js"; acy checkpoint --wait -m "G24 mainline" >/dev/null
BEFORE="$(cat "$R/src/nine.js")"
OUT="$(promote_ok "$A" G24)"
echo "$OUT" | grep -q 'promoted by merge: 1 file' || fail "G24: expected a merge: $OUT"
grep -q '^l3 fork$' "$R/src/nine.js" || fail "G24: merge did not land"
TL="$(acy timeline)"
echo "$TL" | grep -q "merged 1 file(s), replayed 1 path(s) onto moved mainline" || fail "G24: landed row must count the merge: $TL"
echo "$TL" | grep -q "fork $A merge (1 merged, 0 replayed)" || fail "G24: merge generation row: $TL"
SAFETY="$(echo "$TL" | awk '/before promote fork .* \(merge\)/{print $1; exit}' | tr -d '#')"
acy rewind -y "$SAFETY" >/dev/null || fail "G24: rewind to the safety row"
[ "$(cat "$R/src/nine.js")" = "$BEFORE" ] || fail "G24: rewind must restore the pre-merge file: $(cat "$R/src/nine.js")"

# --- G25: CRLF survives a merge through the CLI, byte for byte ---------------
printf 'a\r\nb\r\nc\r\n' > "$R/crlf.txt"; acy checkpoint -m "G25 base" >/dev/null
A="$(fork)"; B="$(fork)"
printf 'A\r\nb\r\nc\r\n' > "$(fork_path "$A")/crlf.txt"
printf 'a\r\nb\r\nC\r\n' > "$(fork_path "$B")/crlf.txt"
promote_ok "$A" G25 >/dev/null
promote_ok "$B" G25 >/dev/null
cmp -s "$R/crlf.txt" <(printf 'A\r\nb\r\nC\r\n') || fail "G25: CRLF bytes changed: $(od -c "$R/crlf.txt")"

# --- G26: one mergeable and one conflicting file: nothing lands, both rebased
printf 'l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\n' > "$R/src/nine.js"
printf 'line1\nline2\nline3\n' > "$R/src/shared.js"; acy checkpoint -m "G26 base" >/dev/null
A="$(fork)"; B="$(fork)"
sed -e 's/^l1$/l1 AA/;s/^l9$/l9 AA/' "$R/src/nine.js" > "$(fork_path "$A")/src/nine.js"
printf 'line1 A\nline2\nline3\n' > "$(fork_path "$A")/src/shared.js"
sed -e 's/^l1$/l1 BB/' "$R/src/nine.js" > "$(fork_path "$B")/src/nine.js"   # conflicts on l1
printf 'line1\nline2\nline3 B\n' > "$(fork_path "$B")/src/shared.js"        # merges cleanly
printf 'ok\n' > "$(fork_path "$B")/src/other.js"
promote_ok "$A" G26 >/dev/null
AFTER_A_NINE="$(cat "$R/src/nine.js")"
OUT="$(promote_refused "$B" G26)"
echo "$OUT" | grep -q '1 file(s) conflict' || fail "G26: exactly one file conflicts: $OUT"
echo "$OUT" | grep -q 'src/nine.js: 1 conflicting hunk(s)' || fail "G26: nine.js conflict: $OUT"
[ "$(cat "$R/src/nine.js")" = "$AFTER_A_NINE" ] || fail "G26: mainline touched"
[ "$(cat "$R/src/shared.js")" = $'line1 A\nline2\nline3' ] || fail "G26: nothing may land, not even the clean merge"
[ ! -e "$R/src/other.js" ] || fail "G26: nothing may land"
has_markers "$(fork_path "$B")/src/nine.js" || fail "G26: conflicting file needs markers in the fork"
grep -q '^l9 AA$' "$(fork_path "$B")/src/nine.js" || fail "G26: the clean hunk of the conflicting file is merged in the fork"
[ "$(cat "$(fork_path "$B")/src/shared.js")" = $'line1 A\nline2\nline3 B' ] || fail "G26: the mergeable file must be merged in the fork (R carries it): $(cat "$(fork_path "$B")/src/shared.js")"
[ "$(cat "$(fork_path "$B")/src/other.js")" = "ok" ] || fail "G26: fork-only file kept"
# Resolving the one conflict lands everything in one promote.
printf 'l1 AB\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9 AA\n' > "$(fork_path "$B")/src/nine.js"
promote_ok "$B" G26 >/dev/null
[ "$(head -1 "$R/src/nine.js")" = "l1 AB" ] || fail "G26: resolution did not land: $(cat "$R/src/nine.js")"
[ "$(cat "$R/src/shared.js")" = $'line1 A\nline2\nline3 B' ] && [ "$(cat "$R/src/other.js")" = "ok" ] || fail "G26: the rest did not land with the resolution"

pass "merge ($MODE mode): disjoint forks replay, same-file edits merge by line, same-line/modify-delete/add-add conflicts rebase the fork with diff3 markers and resolve-then-promote lands, binary/kind/ancestry overlaps are refused leaving the fork untouched, merges are undoable, CRLF survives, all-or-nothing holds"

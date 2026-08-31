#!/usr/bin/env bash
# Capture-fidelity soak: N rapid edit->checkpoint cycles, then every recorded
# generation is materialized and content-verified. This is the test that
# caught the macOS FSEvents metadata-coalescing stale-capture bug.
# Post checkpoints are smeared by design ("at least this tool's effects"),
# so a queued checkpoint may contain the immediately following write; the
# tolerance below encodes exactly that and nothing more.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

ROUNDS="${ACYCLIC_SOAK_ROUNDS:-40}"

setup_repo
printf 'v0\n' > "$R/app.txt"
acy init >/dev/null || fail "init"

for I in $(seq 1 "$ROUNDS"); do
  printf 'v%s\n' "$I" > "$R/app.txt"
  sleep 0.15
  acy checkpoint --kind post >/dev/null || fail "checkpoint $I"
  sleep 0.05
done
settle 2
acy stop >/dev/null
sleep 0.5

VOL="$(python3 -c "import json,glob;print(json.load(open(glob.glob('$STORES/*/meta.json')[0]))['volume_id'].replace('-',''))")"
STORE_DIR="$(ls -d "$STORES"/*/store)"
DB="$(ls "$STORES"/*/index.db)"

BAD=0
CHECKED=0
LAST=""
while IFS=: read -r ID GEN; do
  CHECKED=$((CHECKED + 1))
  PROBE="$WORK/probe"
  rm -rf "$PROBE"
  "$QUAL" restore-gen "$STORE_DIR" "$VOL" "$GEN" "$PROBE" >/dev/null 2>&1 \
    || fail "restore-gen for row $ID"
  GOT="$(cat "$PROBE/app.txt" 2>/dev/null || echo missing)"
  WANT=$((ID - 1))
  if [ "$GOT" != "v$WANT" ] && [ "$GOT" != "v$((WANT + 1))" ]; then
    echo "  row #$ID: got $GOT want v$WANT (or smear v$((WANT + 1)))"
    BAD=$((BAD + 1))
  fi
  LAST="$GOT"
done < <(sqlite3 "$DB" "SELECT id || ':' || lower(hex(generation)) FROM checkpoints WHERE kind IN ('post','noop') ORDER BY id;")

[ "$CHECKED" -eq "$ROUNDS" ] || fail "expected $ROUNDS checkpoint rows, saw $CHECKED"
[ "$BAD" -eq 0 ] || fail "$BAD of $CHECKED captures out of tolerance"
[ "$LAST" = "v$ROUNDS" ] || fail "final capture is $LAST, want v$ROUNDS"

pass "soak: $CHECKED captures content-verified, 0 out of tolerance"

#!/usr/bin/env bash
# Store growth (docs/design/01-rewind.md): a long session of distinct checkpoints
# must cost the same on a big tree as on a small one, i.e. growth is a
# fixed per-checkpoint overhead (tree pages, generation roots) and never a
# copy of the tree. Two repos, same edit sequence, 4x the blob size: the
# growth must match within tolerance and stay small per checkpoint.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

ROUNDS="${ACYCLIC_GROWTH_ROUNDS:-60}"
SMALL_MB="${ACYCLIC_GROWTH_MB:-4}"
BIG_MB=$((SMALL_MB * 4))
PER_CHECKPOINT_CAP=$((128 * 1024))

store_bytes() { du -sk "$STORES"/*/store | cut -f1 | awk '{s+=$1} END {print s*1024}'; }

# run_session <blob_mb>: prints "<tree_bytes> <growth_bytes>"
run_session() {
  local mb="$1"
  acy stop >/dev/null 2>&1 || true
  sleep 1
  rm -rf "$R" "$STORES" 2>/dev/null || true
  setup_repo
  head -c $((mb * 1024 * 1024)) /dev/urandom > "$R/blob.bin"
  printf 'v0\n' > "$R/counter.txt"
  local tree; tree="$(du -sk "$R" | cut -f1)"; tree=$((tree * 1024))
  acy init >/dev/null || fail "init ($mb MiB)"
  acy checkpoint --wait -m baseline >/dev/null || fail "baseline ($mb MiB)"
  acy commit >/dev/null || fail "commit ($mb MiB)"
  local before; before="$(store_bytes)"
  [ "$before" -lt $((tree * 3)) ] || fail "baseline store is >3x the tree ($before vs $tree)"
  local i
  for i in $(seq 1 "$ROUNDS"); do
    printf 'v%s %s\n' "$i" "$RANDOM$RANDOM" > "$R/counter.txt"
    sleep 0.05
    acy checkpoint --wait --kind post >/dev/null || fail "checkpoint $i ($mb MiB)"
  done
  acy commit >/dev/null || fail "final commit ($mb MiB)"
  acy stop >/dev/null || fail "stop ($mb MiB)"
  sleep 0.5
  local after; after="$(store_bytes)"
  echo "$tree $((after - before))"
}

read -r SMALL_TREE SMALL_GROWTH <<<"$(run_session "$SMALL_MB")"
read -r BIG_TREE BIG_GROWTH <<<"$(run_session "$BIG_MB")"
SMALL_PER=$((SMALL_GROWTH / ROUNDS))
BIG_PER=$((BIG_GROWTH / ROUNDS))
echo "  $ROUNDS checkpoints: tree $SMALL_TREE -> +$SMALL_GROWTH (~$SMALL_PER each); tree $BIG_TREE -> +$BIG_GROWTH (~$BIG_PER each)"

[ "$SMALL_PER" -lt "$PER_CHECKPOINT_CAP" ] || fail "per-checkpoint growth $SMALL_PER bytes exceeds $PER_CHECKPOINT_CAP"
[ "$BIG_PER" -lt "$PER_CHECKPOINT_CAP" ] || fail "per-checkpoint growth $BIG_PER bytes exceeds $PER_CHECKPOINT_CAP on the big tree"
# 4x the tree must not mean anywhere near 4x the growth: allow 25% slack.
[ "$BIG_GROWTH" -le $((SMALL_GROWTH + SMALL_GROWTH / 4)) ] \
  || fail "growth scales with tree size: $BIG_GROWTH on the 4x tree vs $SMALL_GROWTH"

pass "growth: ~$SMALL_PER bytes per checkpoint, unchanged on a 4x larger tree (sub-linear in tree size)"

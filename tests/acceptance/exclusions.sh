#!/usr/bin/env bash
# Snapshot exclusions (`exclude` in .acyclic/config.toml): declared paths
# never enter a checkpoint, hint-only changes to them are noops, a single-
# path restore of one is refused, a full rewind carries the LIVE copies into
# the restored tree, and a directory rename that lands on an excluded prefix
# is scrubbed. Also proves the documented limit: a generation captured
# BEFORE the rule existed still holds the path (exclusion is not purge).
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

setup_repo
mkdir -p "$R/secrets" "$R/staging"
printf 'KEY\n' > "$R/secrets/key.pem"
printf 'STAGED\n' > "$R/staging/token"

# --- Phase A: no rule yet. The baseline captures .env (the documented gap).
acy init >/dev/null || fail "init"
acy checkpoint --wait -m "before exclusions" >/dev/null || fail "checkpoint A"
acy stop >/dev/null || fail "stop"
sleep 0.5

VOL="$(python3 -c "import json,glob;print(json.load(open(glob.glob('$STORES/*/meta.json')[0]))['volume_id'].replace('-',''))")"
STORE_DIR="$(ls -d "$STORES"/*/store)"
DB="$(ls "$STORES"/*/index.db)"
gen_of() { sqlite3 "$DB" "SELECT hex(generation) FROM checkpoints WHERE id=$1;" | tr 'A-F' 'a-f'; }
latest_real() { sqlite3 "$DB" "SELECT id FROM checkpoints WHERE kind IN ('baseline','manual','post','pre','recovered') ORDER BY id DESC LIMIT 1;"; }
# The store is single-owner while the daemon runs: stop it before another
# process reads a generation (the next acy command autospawns a fresh one).
probe_gen() {
  local gen="$1" probe="$WORK/probe"
  acy stop >/dev/null 2>&1 || true
  sleep 0.5
  rm -rf "$probe"
  "$QUAL" restore-gen "$STORE_DIR" "$VOL" "$gen" "$probe" >/dev/null 2>&1 || fail "restore-gen $gen"
  echo "$probe"
}
EARLY_ID="$(latest_real)"
EARLY_GEN="$(gen_of "$EARLY_ID")"
P="$(probe_gen "$EARLY_GEN")"
[ -f "$P/.env" ] || fail "pre-rule baseline should hold .env (nothing excluded yet)"

# --- Phase B: add the rule; the next daemon start scrubs the baseline.
printf 'exclude = [".env", "secrets/"]\n' >> "$R/.acyclic/config.toml"
acy checkpoint --wait -m "after rule" >/dev/null || fail "checkpoint B (autospawn)"
sleep 0.3
B_ID="$(latest_real)"
P="$(probe_gen "$(gen_of "$B_ID")")"
[ ! -e "$P/.env" ] || fail "baseline after the rule still holds .env"
[ ! -e "$P/secrets" ] || fail "baseline after the rule still holds secrets/"
[ -f "$P/staging/token" ] || fail "unrelated path lost from the baseline"
[ "$(cat "$P/src/main.rs")" = "ORIGINAL" ] || fail "tree content wrong in scrubbed baseline"

# The pre-rule generation is untouched: exclusion is not purge.
P="$(probe_gen "$EARLY_GEN")"
[ -f "$P/.env" ] || fail "pre-rule generation changed (exclusion must not rewrite history)"

# --- Phase C: hint-only changes to excluded paths are noops.
printf 'SECRET=2\n' > "$R/.env"
printf 'KEY2\n' > "$R/secrets/key.pem"
settle 0.4
acy checkpoint --wait --kind post >/dev/null || fail "checkpoint C"
KIND="$(sqlite3 "$DB" "SELECT kind FROM checkpoints ORDER BY id DESC LIMIT 1;")"
[ "$KIND" = "noop" ] || fail "excluded-only edits produced a '$KIND' checkpoint, want noop"

# A mixed change captures the included path and still nothing excluded.
printf 'EDITED\n' > "$R/src/main.rs"
settle 0.4
acy checkpoint --wait --kind post >/dev/null || fail "checkpoint C2"
C2_ID="$(latest_real)"
P="$(probe_gen "$(gen_of "$C2_ID")")"
[ "$(cat "$P/src/main.rs")" = "EDITED" ] || fail "included edit not captured"
[ ! -e "$P/.env" ] || fail ".env leaked into a checkpoint"
[ ! -e "$P/secrets" ] || fail "secrets/ leaked into a checkpoint"
# Between two post-rule checkpoints the excluded edits are invisible.
DIFF="$(acy diff "$B_ID" "$C2_ID")"
echo "$DIFF" | grep -q "src/main.rs" || fail "diff lost the included edit: $DIFF"
echo "$DIFF" | grep -q "\.env\|secrets" && fail "diff mentions an excluded path: $DIFF"

# --- Phase D: restore of an excluded path is refused with a clear reason.
set +e
OUT="$(acy restore "$B_ID" .env 2>&1)"
CODE=$?
set -e
[ "$CODE" -ne 0 ] || fail "restore of an excluded path succeeded"
echo "$OUT" | grep -q "excluded" || fail "restore refusal does not say why: $OUT"

# --- Phase E: a rename INTO an excluded prefix is scrubbed; the old name
# is re-examined so the checkout does not keep a stale copy.
mv "$R/staging" "$R/secrets/staging"
settle 0.4
acy checkpoint --wait --kind post >/dev/null || fail "checkpoint E"
E_ID="$(latest_real)"
P="$(probe_gen "$(gen_of "$E_ID")")"
[ ! -e "$P/staging" ] || fail "renamed-away directory still in the checkpoint"
[ ! -e "$P/secrets" ] || fail "rename into the excluded prefix was captured"

# --- Phase F: full rewind carries the LIVE excluded copies over.
acy rewind "$B_ID" --yes >/dev/null || fail "rewind"
[ "$(cat "$R/src/main.rs")" = "ORIGINAL" ] || fail "rewind did not restore the tree"
[ "$(cat "$R/.env")" = "SECRET=2" ] || fail "rewind lost or reverted the live .env: $(cat "$R/.env" 2>&1)"
[ "$(cat "$R/secrets/key.pem")" = "KEY2" ] || fail "rewind lost the live secrets/"
[ "$(cat "$R/secrets/staging/token")" = "STAGED" ] || fail "rewind lost content moved under the excluded prefix"
[ "$(cat "$R/staging/token")" = "STAGED" ] || fail "rewind did not restore staging/ as checkpoint B held it"
# The post-rewind baseline is scrubbed too.
settle 0.5
acy checkpoint --wait -m "after rewind" >/dev/null || fail "checkpoint F"
P="$(probe_gen "$(gen_of "$(latest_real)")")"
[ ! -e "$P/.env" ] || fail ".env in the post-rewind baseline"

# Rewinding to the PRE-rule generation: the tree comes back, but the live
# .env still wins over the one that generation holds.
acy rewind "$EARLY_ID" --yes >/dev/null || fail "rewind to pre-rule generation"
[ "$(cat "$R/.env")" = "SECRET=2" ] || fail "live .env overwritten by a pre-rule generation"

pass "exclusions: never captured, noop on excluded-only edits, restore refused, rename scrubbed, rewind carries live copies, pre-rule history untouched"

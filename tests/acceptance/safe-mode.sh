#!/usr/bin/env bash
# Launch 4 Safe Mode acceptance (spec: plans/04-safe-mode.md): session
# redirection through a shadow mount at the repo root, filesystem-level
# guarded paths that hold against arbitrary shell, approval-gated
# apply/discard with zero trace, conflict legibility, and crash sweep.
# Skips (exit 0) when native mounts are unavailable unless
# ACYCLIC_FORKS_REQUIRED=1.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

skip() {
  if [ "${ACYCLIC_FORKS_REQUIRED:-0}" = "1" ]; then
    fail "$*"
  fi
  echo "SKIP($(basename "$0")): $*"
  exit 0
}

# Every mount is torn down on exit even on failure: the shadow mount sits on
# the real repo root, so a leak here poisons every later script.
safe_teardown() {
  acy stop >/dev/null 2>&1 || true
  sleep 0.3
  if mount | grep -q " $R "; then
    umount -f "$R" 2>/dev/null || diskutil unmount force "$R" >/dev/null 2>&1 || true
  fi
  teardown
}
trap safe_teardown EXIT

# Environment hygiene (mirrors forks.sh): a kill -9'd daemon anywhere orphans
# its go-nfsv4 helper, and orphaned helpers wedge FUSE-T's tiny shared NFS
# port pool for every later mount — which this script does many of, including
# a crash-recovery remount. Reap acyclic helpers + mounts up front.
if [ "$(uname -s)" = "Darwin" ]; then
  pkill -f 'go-nfsv4.*acyclic-fs' 2>/dev/null || true
  sleep 0.5
  mount | awk '/fuse-t:\/acyclic-fs|127\.0\.0\.1:\//{print $3}' | while read -r M; do
    case "$M" in */acyclic-acceptance.*) umount -f "$M" 2>/dev/null || true ;; esac
  done
fi

shadowed() { mount | grep -q " $R "; }
sha() { shasum -a 256 "$1" | awk '{print $1}'; }
tree_digest() {
  (cd "$1" && find . -type f ! -path './.acyclic/*' -print0 | sort -z \
    | xargs -0 shasum -a 256 | shasum -a 256 | awk '{print $1}')
}
# A guarded command must be REFUSED, but on the macOS loopback-NFS transport
# a rejected write can still return exit 0 to the shell (client write-back
# caching acks the write before the server refuses it). So a guard assertion
# must verify the *effect* was prevented, not the command's exit status —
# `must_fail` is only for control-plane commands (session/apply) that reply
# synchronously. Guarded-write refusals are checked by content below.
must_fail() {
  local label="$1"; shift
  if "$@" 2>"$WORK/err"; then
    fail "$label: succeeded but must be refused"
  fi
}

setup_repo
mkdir -p "$R/migrations" "$R/src/nested"
printf 'CREATE TABLE a;\n' > "$R/migrations/001.sql"
printf 'deep\n' > "$R/src/nested/deep.txt"
printf 'ORIGINAL-B\n' > "$R/src/b.rs"
cat >> "$R/.acyclic/config.toml" <<'EOF'
dry_run = true
guarded_paths = [".env", "migrations/"]
EOF
acy init >/dev/null || fail "init"
REAL_BEFORE="$(tree_digest "$R")"
INODE_BEFORE="$(stat -f '%i' "$R")"

# --- S1: session start shadows the repo root; identical to the agent ------
if ! OUT="$(acy session-start dry-1 --host acceptance 2>&1)"; then
  echo "$OUT" | grep -qi 'mount' && skip "native mounts unavailable: $OUT"
  fail "session-start: $OUT"
fi
shadowed || fail "S1: repo root is not shadow-mounted after session-start"
[ "$(cat "$R/src/main.rs")" = "ORIGINAL" ] || fail "S1: shadow does not serve the tree"
[ "$(cat "$R/.env")" = "SECRET=1" ] || fail "S1: gitignored file missing from shadow"
[ "$(cat "$R/migrations/001.sql")" = "CREATE TABLE a;" ] || fail "S1: guarded file unreadable"
[ "$(cat "$R/src/nested/deep.txt")" = "deep" ] || fail "S1: nested path missing"
[ "$(ls "$R" | sort | tr '\n' ' ')" = "$(printf 'migrations src \n' )" ] \
  || fail "S1: listing differs from real tree: $(ls "$R" | tr '\n' ' ')"

# --- S2: a second Safe Mode session is refused while one is active --------
must_fail "S2: second session-start" acy session-start dry-2 --host acceptance
grep -qi "already active" "$WORK/err" || fail "S2: refusal not legible: $(cat "$WORK/err")"

# --- S3: ordinary writes land in the shadow, at every depth ---------------
printf 'MIGRATED\n' > "$R/src/main.rs" || fail "S3: write main.rs"
printf 'deeper\n' > "$R/src/nested/deep.txt" || fail "S3: nested write"
mkdir -p "$R/src/new/dir" || fail "S3: mkdir"
printf 'fresh\n' > "$R/src/new/dir/file.txt" || fail "S3: create in new dir"
rm "$R/src/b.rs" || fail "S3: rm"
head -c 4096 /dev/zero > "$R/generated.bin" || fail "S3: generate artifact"
mv "$R/src/nested/deep.txt" "$R/src/nested/moved.txt" || fail "S3: rename"
[ "$(cat "$R/src/main.rs")" = "MIGRATED" ] || fail "S3: readback main.rs"
[ "$(cat "$R/src/nested/moved.txt")" = "deeper" ] || fail "S3: readback rename"
[ ! -e "$R/src/b.rs" ] || fail "S3: rm not reflected"

# --- S4: guarded paths refuse EVERY mutation from arbitrary shell ---------
# Each mutation is attempted through plain shell; the exit code is ignored
# (macOS NFS write-back may ack a doomed write), so refusal is proven by the
# guarded content/structure being byte-identical afterward.
try() { "$@" >/dev/null 2>&1 || true; }
try bash -c "printf 'LEAK=1\n'  > '$R/.env'"
try bash -c "printf 'LEAK=1\n' >> '$R/.env'"
try bash -c ": > '$R/.env'"
try rm "$R/.env"
try mv "$R/.env" "$R/env.bak"
try bash -c "printf 'x\n' > '$R/tmp.txt' && mv '$R/tmp.txt' '$R/.env'"
try chmod 600 "$R/.env"
try bash -c "printf 'DROP TABLE a;\n' > '$R/migrations/001.sql'"
try bash -c "printf 'x\n' > '$R/migrations/002.sql'"
try mkdir "$R/migrations/sub"
try rm "$R/migrations/001.sql"
try bash -c "rm -rf '$R/migrations'"
try bash -c "mv '$R/generated.bin' '$R/migrations/'"
try mv "$R/migrations" "$R/old-migrations"
rm -f "$R/tmp.txt"
# The refusals were real: guarded content and structure are byte-identical.
[ "$(cat "$R/.env")" = "SECRET=1" ] || fail "S4: .env changed: $(cat "$R/.env")"
[ "$(cat "$R/migrations/001.sql")" = "CREATE TABLE a;" ] || fail "S4: migration changed"
[ ! -e "$R/migrations/002.sql" ] || fail "S4: file created in guarded dir"
[ ! -e "$R/migrations/sub" ] || fail "S4: dir created in guarded dir"
[ -e "$R/generated.bin" ] || fail "S4: artifact vanished during refused move"
[ -e "$R/migrations" ] || fail "S4: guarded dir vanished"
[ ! -e "$R/env.bak" ] && [ ! -e "$R/old-migrations" ] || fail "S4: guarded rename escaped"
# Reads under guard keep working, and a sibling path (prefix, not guarded) does too.
printf 'ok\n' > "$R/migrations-notes.txt" || fail "S4: prefix-sibling path wrongly guarded"
[ "$(cat "$R/.env")" = "SECRET=1" ] || fail "S4: guarded read broken"

# --- S5: the real tree is untouched underneath, and hooks keep working -----
# Checkpoints taken mid-session (hooks fire as normal) must neither wedge the
# daemon nor pollute the mainline timeline with fork content.
acy checkpoint --wait --kind post --session-id dry-1 --tool-name Bash >/dev/null \
  || fail "S5: checkpoint during shadowed session"
STATUS="$(acy status)"
echo "$STATUS" | grep -q "state:         ready" || fail "S5: daemon not ready mid-session: $STATUS"

# --- S6: resolve unmounts, restores the real view, shows the exact diff ---
DIFF="$(acy session-resolve dry-1)" || fail "S6: session-resolve: $DIFF"
shadowed && fail "S6: still shadow-mounted after resolve"
[ "$(stat -f '%i' "$R")" = "$INODE_BEFORE" ] || fail "S6: real root inode changed"
[ "$(tree_digest "$R")" = "$REAL_BEFORE" ] || fail "S6: real tree changed before apply"
echo "$DIFF" | grep -q "^M src/main.rs" || fail "S6: diff missing edit: $DIFF"
echo "$DIFF" | grep -q "^D src/b.rs" || fail "S6: diff missing rm: $DIFF"
echo "$DIFF" | grep -q "^A generated.bin" || fail "S6: diff missing artifact: $DIFF"
echo "$DIFF" | grep -q "^A src/new/dir/file.txt" || fail "S6: diff missing nested create: $DIFF"
echo "$DIFF" | grep -q "^A src/nested/moved.txt" || fail "S6: diff missing rename target: $DIFF"
echo "$DIFF" | grep -q "^D src/nested/deep.txt" || fail "S6: diff missing rename source: $DIFF"
echo "$DIFF" | grep -q "^A migrations-notes.txt" || fail "S6: diff missing sibling: $DIFF"
echo "$DIFF" | grep -q "\.env" && fail "S6: guarded .env in diff: $DIFF"
echo "$DIFF" | grep -q "migrations/" && fail "S6: guarded dir in diff: $DIFF"
echo "$DIFF" | grep -q "session-apply dry-1" || fail "S6: no apply instruction: $DIFF"
must_fail "S6: resolve twice" acy session-resolve dry-1
must_fail "S6: resolve unknown" acy session-resolve nope

# --- S7: discard leaves zero trace; a fresh session starts clean ----------
acy session-discard dry-1 >/dev/null || fail "S7: session-discard"
[ "$(tree_digest "$R")" = "$REAL_BEFORE" ] || fail "S7: discard touched the real tree"
must_fail "S7: apply after discard" acy session-apply dry-1
acy session-start dry-2 --host acceptance >/dev/null || fail "S7: new session after discard"
shadowed || fail "S7: second session not shadowed"
[ "$(cat "$R/src/main.rs")" = "ORIGINAL" ] || fail "S7: discarded edit leaked into next session"
[ ! -e "$R/generated.bin" ] || fail "S7: discarded artifact leaked into next session"

# --- S8: a session with no writes resolves as no changes -----------------
OUT="$(acy session-resolve dry-2)" || fail "S8: resolve empty session"
echo "$OUT" | grep -q "no changes" || fail "S8: expected 'no changes': $OUT"
shadowed && fail "S8: still mounted after empty resolve"
[ "$(tree_digest "$R")" = "$REAL_BEFORE" ] || fail "S8: empty session changed the tree"

# --- S9: apply lands atomically; guarded content survives ----------------
acy session-start dry-3 --host acceptance >/dev/null || fail "S9: session-start"
printf 'APPLIED\n' > "$R/src/main.rs"
printf 'brand new\n' > "$R/new.txt"
rm "$R/src/b.rs"
acy session-resolve dry-3 >/dev/null || fail "S9: resolve"
[ "$(cat "$R/src/main.rs")" = "ORIGINAL" ] || fail "S9: real tree changed before apply"
OUT="$(acy session-apply dry-3)" || fail "S9: session-apply: $OUT"
shadowed && fail "S9: mounted after apply"
[ "$(cat "$R/src/main.rs")" = "APPLIED" ] || fail "S9: edit not applied"
[ "$(cat "$R/new.txt")" = "brand new" ] || fail "S9: new file not applied"
[ ! -e "$R/src/b.rs" ] || fail "S9: rm not applied"
[ "$(cat "$R/.env")" = "SECRET=1" ] || fail "S9: guarded .env changed by apply"
[ "$(cat "$R/migrations/001.sql")" = "CREATE TABLE a;" ] || fail "S9: guarded migration changed"
[ -d "$R/.acyclic" ] || fail "S9: repo config lost by apply"
must_fail "S9: apply twice" acy session-apply dry-3
# The applied tree is the daemon's new mainline: a checkpoint/rewind cycle
# round-trips it, and the old tree is retained for the trash TTL.
acy checkpoint --wait --kind post >/dev/null || fail "S9: checkpoint after apply"
echo "$OUT" | grep -q "reload your editor" || fail "S9: no editor warning: $OUT"

# --- S10: apply conflicts legibly when the mainline moved -----------------
acy session-start dry-4 --host acceptance >/dev/null || fail "S10: session-start"
printf 'FORK-EDIT\n' > "$R/src/main.rs"
acy session-resolve dry-4 >/dev/null || fail "S10: resolve"
printf 'MAINLINE-EDIT\n' > "$R/src/main.rs"
settle 0.4
acy checkpoint --wait --durable --kind post >/dev/null || fail "S10: mainline checkpoint"
set +e
OUT="$(acy session-apply dry-4 2>&1)"
CODE=$?
set -e
[ "$CODE" -ne 0 ] || fail "S10: apply succeeded over a moved mainline: $OUT"
echo "$OUT" | grep -qi "conflict\|moved\|behind" || fail "S10: conflict not legible: $OUT"
[ "$(cat "$R/src/main.rs")" = "MAINLINE-EDIT" ] || fail "S10: conflicting apply touched the tree"

# --- S11: guarded paths also hold on explicit forks ----------------------
# Verified via server truth (promote), not client reads: the macOS NFS
# client caches a doomed write and would echo it back on `cat`, so a
# guarded write's refusal shows only in what promote actually lands.
FORK_OUT="$(acy fork -n 1)" || fail "S11: fork"
FID="$(echo "$FORK_OUT" | awk '/^fork /{print $2}')"
F="$(dirname "$R")/.$(basename "$R").forks/mnt/$FID"
[ -f "$F/.env" ] || fail "S11: fork missing .env"
try bash -c "printf 'LEAK\n' > '$F/.env'"       # guarded — must not land
try rm "$F/migrations/001.sql"                   # guarded — must not land
printf 'S11-FORK\n' > "$F/src/main.rs" || fail "S11: fork unguarded write"
settle 0.4
acy promote "$FID" >/dev/null || fail "S11: promote"
# Only the unguarded edit reached the real tree; guarded paths are intact.
[ "$(cat "$R/src/main.rs")" = "S11-FORK" ] || fail "S11: unguarded edit not landed"
[ "$(cat "$R/.env")" = "SECRET=1" ] || fail "S11: guarded .env landed a change"
[ "$(cat "$R/migrations/001.sql")" = "CREATE TABLE a;" ] || fail "S11: guarded migration changed"

# --- S12: stop unmounts an active shadow; nothing is left on the root -----
acy session-start dry-5 --host acceptance >/dev/null || fail "S12: session-start"
printf 'LOST\n' > "$R/src/main.rs"
shadowed || fail "S12: not shadowed"
acy stop >/dev/null || fail "S12: stop"
sleep 0.5
shadowed && fail "S12: shadow left mounted after stop"
[ "$(cat "$R/src/main.rs")" = "S11-FORK" ] || fail "S12: stop leaked shadow writes: $(cat "$R/src/main.rs")"

# --- S13: kill -9 mid-session: the next daemon sweeps the dead shadow -----
acy status >/dev/null || fail "S13: respawn"
acy session-start dry-6 --host acceptance >/dev/null || fail "S13: session-start"
printf 'LOST-2\n' > "$R/src/main.rs"
shadowed || fail "S13: not shadowed"
PID="$(daemon_pid)"
[ -n "$PID" ] || fail "S13: no daemon pid"
kill -9 "$PID"
sleep 0.5
# A dead NFS server behind the mount: the real tree must come back on the
# next daemon start, without a manual umount.
acy status >/dev/null || fail "S13: daemon restart with dead shadow"
sleep 0.3
shadowed && fail "S13: dead shadow not swept"
[ "$(cat "$R/src/main.rs")" = "S11-FORK" ] || fail "S13: crash leaked shadow writes: $(cat "$R/src/main.rs")"
[ "$(cat "$R/.env")" = "SECRET=1" ] || fail "S13: .env after crash"
must_fail "S13: resolve dead session" acy session-resolve dry-6
# And the repo is fully usable again: a new session works end to end.
acy session-start dry-7 --host acceptance >/dev/null || fail "S13: session after sweep"
printf 'AFTER-CRASH\n' > "$R/src/main.rs"
acy session-resolve dry-7 | grep -q "^M src/main.rs" || fail "S13: resolve after sweep"
acy session-apply dry-7 >/dev/null || fail "S13: apply after sweep"
[ "$(cat "$R/src/main.rs")" = "AFTER-CRASH" ] || fail "S13: apply after sweep"

pass "safe-mode: shadow session, guarded paths vs shell, resolve/discard/apply, conflict, stop + crash sweep"

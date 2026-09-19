#!/usr/bin/env bash
# Windows end-to-end smoke: the paths that are platform-specific enough to
# break silently there, driven through the real CLI and daemon.
#
# The rest of the acceptance suite assumes a POSIX host (mount tooling,
# symlinks, modes) and is not claimed to run here. This script covers what
# Windows actually changes:
#
#   - the named-pipe transport, including a client whose stdout is a pipe
#     (a daemon that inherits it never lets the caller see EOF)
#   - capture and diff under UTF-16LE names, non-ASCII included
#   - rewind, which renames the repo root and so trips every open handle
#   - required ProjFS-accelerated forks
#
# Runs under Git Bash on a GitHub windows runner. ACYCLIC_BIN overrides the
# binary (default: target/release/acyclic.exe).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
ACYCLIC="${ACYCLIC_BIN:-$ROOT/target/release/acyclic.exe}"
[ -x "$ACYCLIC" ] || { echo "windows-smoke: no binary at $ACYCLIC" >&2; exit 1; }

WORK="$(mktemp -d -t acyclic-windows-smoke.XXXXXXXX)"
WORK="$(cd "$WORK" && pwd -P)"
REPO="$WORK/repo"
cleanup() {
  local status=$?
  if [ "$status" -ne 0 ] && [ "${ACYCLIC_SMOKE_METRICS:-0}" = 1 ]; then
    /usr/bin/find "$WORK/stores" -name daemon.log -exec tail -n 80 {} \; 2>/dev/null || true
  fi
  "$ACYCLIC" --repo "$REPO" stop >/dev/null 2>&1 || true
  if [[ "$WORK" == */acyclic-windows-smoke.* && -d "$WORK" ]]; then
    rm -rf -- "$WORK" 2>/dev/null || true
  fi
}
trap cleanup EXIT

fail() { echo "windows-smoke: $1" >&2; exit 1; }

metric() {
  [ "${ACYCLIC_SMOKE_METRICS:-0}" = 1 ] || return 0
  local now
  now="$(date +%s%N)"
  echo "windows-smoke metric $1 ms=$(((now - metric_start) / 1000000))"
  metric_start="$now"
}

mkdir -p "$REPO/src"
mkdir -p "$REPO/.acyclic"
# Keep the daemon's durable store inside this disposable fixture, not under
# the runner account's persistent HOME.
printf 'store_dir = "%s"\n' "$(cygpath -m "$WORK/stores")" > "$REPO/.acyclic/config.toml"
cd "$REPO"
git init -q .
git config user.email smoke@example.com
git config user.name smoke
printf 'ORIGINAL CONTENT LINE\n' > src/main.rs
printf 'base\n' > a.txt
# A non-ASCII name: with the wrong name encoding this is where capture or
# the diff walk starts producing mojibake instead of failing outright.
printf 'unicode\n' > "ünïcøde.txt"
git add -A
git commit -qm init
metric_start="$(date +%s%N)"

echo "--- init (stdout through a pipe: must not hang)"
# `| cat` is the whole point: a daemon holding an inherited stdout handle
# leaves this blocked forever rather than returning.
"$ACYCLIC" init < /dev/null | cat > /dev/null || fail "init failed"
metric init

echo "--- the daemon pipe is reachable only by this account"
powershell -NoProfile -ExecutionPolicy Bypass -File "$HERE/windows-pipe-acl.ps1" \
  || fail "the daemon pipe is not owner-only"
metric pipe_acl

echo "--- checkpoint and timeline"
# `init` pings the pipeline, whose reply is queued behind its baseline.
"$ACYCLIC" timeline < /dev/null | grep -q baseline \
  || fail "init returned before the baseline was ready"
printf 'DIFFERENT AND LONGER CONTENT\n' > src/main.rs
printf 'extra\n' > added.txt
"$ACYCLIC" checkpoint < /dev/null > /dev/null || fail "checkpoint failed"
"$ACYCLIC" timeline < /dev/null | grep -q baseline || fail "timeline has no baseline"
metric checkpoint

echo "--- diff names the changed paths"
diff_out="$("$ACYCLIC" diff 1 2 < /dev/null)"
grep -q 'added.txt' <<< "$diff_out" || fail "diff missed added.txt: $diff_out"
grep -qi 'main.rs' <<< "$diff_out" || fail "diff missed main.rs: $diff_out"
metric diff

echo "--- rewind restores content and drops added files"
# `rewind` asks for confirmation on a tty; feed it the answer rather than
# closing stdin, which would read as "abort".
echo y | "$ACYCLIC" rewind 1 > "$WORK/rewind.log" 2>&1 \
  || fail "rewind failed: $(cat "$WORK/rewind.log")"
grep -q 'ORIGINAL CONTENT LINE' src/main.rs || fail "rewind did not restore main.rs"
[ ! -e added.txt ] || fail "rewind left added.txt behind"
[ -e "ünïcøde.txt" ] || fail "rewind lost the non-ASCII path"
[ -d .git ] || fail "rewind lost .git"
metric rewind

echo "--- fork writes and promote land"
"$ACYCLIC" fork -n 1 < /dev/null > /dev/null || fail "fork failed"
fork_row="$("$ACYCLIC" forks < /dev/null | head -1)"
fork_id="$(awk '{print $1}' <<< "$fork_row")"
fork_mode="$(awk '{print $2}' <<< "$fork_row")"
[ -n "$fork_id" ] || fail "no fork id"
case "$fork_mode" in
  mount) fork_dir="$(dirname "$REPO")/.$(basename "$REPO").forks/mnt/$fork_id" ;;
  copy) fork_dir="$(dirname "$REPO")/.$(basename "$REPO").forks/copy/$fork_id" ;;
  *) fail "Windows fork reported an unknown mode: $fork_row" ;;
esac
[ -d "$fork_dir" ] || fail "no $fork_mode fork directory at $fork_dir"
metric "${fork_mode}_fork"
printf 'FORK WORK\n' > "$fork_dir/note.txt"
printf 'edited in fork\n' > "$fork_dir/a.txt"
"$ACYCLIC" fork-diff "$fork_id" < /dev/null | grep -q 'note.txt' \
  || fail "fork-diff did not see the fork's write"
metric fork_diff
"$ACYCLIC" promote "$fork_id" < /dev/null > /dev/null || fail "promote failed"
grep -q 'FORK WORK' note.txt || fail "promote did not land note.txt"
grep -q 'edited in fork' a.txt || fail "promote did not land a.txt"
metric promote

if [ "${ACYCLIC_SMOKE_METRICS:-0}" = 1 ]; then
  pid_file="$(/usr/bin/find "$WORK/stores" -name daemon.pid -print -quit)"
  if [ -n "$pid_file" ]; then
    ACYCLIC_SMOKE_PID="$(< "$pid_file")" powershell -NoProfile -Command \
      '$p = Get-Process -Id $env:ACYCLIC_SMOKE_PID; "windows-smoke resource cpu_ms={0} working_set_bytes={1} private_bytes={2}" -f [int64]($p.CPU * 1000), $p.WorkingSet64, $p.PrivateMemorySize64'
  fi
  echo "windows-smoke resource store_kib=$(du -sk "$WORK/stores" | awk '{print $1}')"
  metric_start="$(date +%s%N)"
fi

echo "--- daemon restart retains promoted files and timeline"
"$ACYCLIC" stop < /dev/null > /dev/null || fail "stop failed"
"$ACYCLIC" timeline < /dev/null | grep -q baseline \
  || fail "timeline did not recover after restart"
grep -q 'FORK WORK' note.txt || fail "restart lost promoted note.txt"
grep -q 'edited in fork' a.txt || fail "restart lost promoted a.txt"
metric restart

echo "--- a second daemon refuses the store"
if "$ACYCLIC" __daemon "$REPO" < /dev/null > /dev/null 2>&1; then
  fail "a second daemon started against a live store"
fi
metric second_daemon_refusal

echo "windows-smoke: green"

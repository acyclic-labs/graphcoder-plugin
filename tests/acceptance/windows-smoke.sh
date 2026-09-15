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
#   - copy-mode forks and promote, the fork path Windows actually uses
#
# Runs under Git Bash on a GitHub windows runner. ACYCLIC_BIN overrides the
# binary (default: target/release/acyclic.exe).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
ACYCLIC="${ACYCLIC_BIN:-$ROOT/target/release/acyclic.exe}"
[ -x "$ACYCLIC" ] || { echo "windows-smoke: no binary at $ACYCLIC" >&2; exit 1; }

WORK="$(mktemp -d)"
REPO="$WORK/repo"
cleanup() {
  "$ACYCLIC" --repo "$REPO" stop >/dev/null 2>&1 || true
  # A daemon that outlives the run holds the store open and fails the next one.
  powershell -NoProfile -Command \
    "Stop-Process -Name acyclic -Force -ErrorAction SilentlyContinue" >/dev/null 2>&1 || true
  rm -rf "$WORK" 2>/dev/null || true
}
trap cleanup EXIT

fail() { echo "windows-smoke: $1" >&2; exit 1; }

mkdir -p "$REPO/src"
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

echo "--- init (stdout through a pipe: must not hang)"
# `| cat` is the whole point: a daemon holding an inherited stdout handle
# leaves this blocked forever rather than returning.
"$ACYCLIC" init < /dev/null | cat > /dev/null || fail "init failed"

echo "--- the daemon pipe is reachable only by this account"
powershell -NoProfile -ExecutionPolicy Bypass -File "$HERE/windows-pipe-acl.ps1" \
  || fail "the daemon pipe is not owner-only"

echo "--- checkpoint and timeline"
# Let the baseline settle before editing. `init` returning does not guarantee
# the baseline predates a write landing microseconds later: the change gets
# folded into checkpoint #1 rather than appearing as a later one, and the diff
# below then has nothing to report. That race is not Windows-specific, and is
# not what this script is here to test.
sleep 3
printf 'DIFFERENT AND LONGER CONTENT\n' > src/main.rs
printf 'extra\n' > added.txt
sleep 2
"$ACYCLIC" checkpoint < /dev/null > /dev/null || fail "checkpoint failed"
"$ACYCLIC" timeline < /dev/null | grep -q baseline || fail "timeline has no baseline"

echo "--- diff names the changed paths"
diff_out="$("$ACYCLIC" diff 1 2 < /dev/null)"
grep -q 'added.txt' <<< "$diff_out" || fail "diff missed added.txt: $diff_out"
grep -qi 'main.rs' <<< "$diff_out" || fail "diff missed main.rs: $diff_out"

echo "--- rewind restores content and drops added files"
# `rewind` asks for confirmation on a tty; feed it the answer rather than
# closing stdin, which would read as "abort".
echo y | "$ACYCLIC" rewind 1 > "$WORK/rewind.log" 2>&1 \
  || fail "rewind failed: $(cat "$WORK/rewind.log")"
grep -q 'ORIGINAL CONTENT LINE' src/main.rs || fail "rewind did not restore main.rs"
[ ! -e added.txt ] || fail "rewind left added.txt behind"
[ -e "ünïcøde.txt" ] || fail "rewind lost the non-ASCII path"
[ -d .git ] || fail "rewind lost .git"

echo "--- forks are copies here, and promote lands them"
"$ACYCLIC" fork -n 1 < /dev/null > /dev/null || fail "fork failed"
fork_id="$("$ACYCLIC" forks < /dev/null | head -1 | awk '{print $1}')"
[ -n "$fork_id" ] || fail "no fork id"
fork_dir="$(dirname "$REPO")/.$(basename "$REPO").forks/copy/$fork_id"
[ -d "$fork_dir" ] || fail "no copy-fork directory at $fork_dir"
printf 'FORK WORK\n' > "$fork_dir/note.txt"
printf 'edited in fork\n' > "$fork_dir/a.txt"
"$ACYCLIC" fork-diff "$fork_id" < /dev/null | grep -q 'note.txt' \
  || fail "fork-diff did not see the fork's write"
"$ACYCLIC" promote "$fork_id" < /dev/null > /dev/null || fail "promote failed"
grep -q 'FORK WORK' note.txt || fail "promote did not land note.txt"
grep -q 'edited in fork' a.txt || fail "promote did not land a.txt"

echo "--- a second daemon refuses the store"
if "$ACYCLIC" __daemon "$REPO" < /dev/null > /dev/null 2>&1; then
  fail "a second daemon started against a live store"
fi

echo "windows-smoke: green"

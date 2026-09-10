#!/usr/bin/env bash
# Shared setup for acceptance scripts. Each script gets an isolated repo and
# store under a temp root, and a daemon that is always torn down on exit.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${ACYCLIC_BIN:-$REPO_ROOT/target/debug/acyclic}"
QUAL="${ACYCLIC_QUAL:-$REPO_ROOT/target/debug/acyclic-qual}"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/acyclic-acceptance.XXXXXX")"
# Canonicalize: macOS TMPDIR ends in "/" and /var -> /private/var, so the
# raw path never string-matches what the mount table prints.
WORK="$(cd "$WORK" && pwd -P)"
R="$WORK/repo"
STORES="$WORK/stores"

fail() {
  EXPLICIT_FAIL=1
  echo "FAIL($(basename "$0")): $*" >&2
  # The daemon's stderr is the only record of a panic; show its tail.
  for log in "$STORES"/*/daemon.log; do
    [ -f "$log" ] && { echo "--- daemon.log (tail) ---" >&2; tail -40 "$log" >&2; }
  done
  exit 1
}

pass() {
  echo "PASS($(basename "$0")): $*"
}

# Portable inode of a path (macOS stat and GNU stat disagree on flags).
inode() {
  case "$(uname)" in
    Darwin) stat -f %i "$1" ;;
    *) stat -c %i "$1" ;;
  esac
}

# `set -e` exits silently on an unexpected command failure. Report it from
# the EXIT path (an ERR trap also fires for failures the script expects
# inside `if` tests), so a CI log names the command that died rather than
# just "N script(s) failed".
EXPLICIT_FAIL=0

acy() {
  "$BIN" --repo "$R" "$@"
}

daemon_pid() {
  cat "$STORES"/*/daemon.pid 2>/dev/null || true
}

teardown() {
  local code=$? cmd="$BASH_COMMAND"
  if [ "$code" -ne 0 ] && [ "${EXPLICIT_FAIL:-0}" -eq 0 ]; then
    echo "FAIL($(basename "$0")): unexpected exit $code from: $cmd" >&2
    for log in "$STORES"/*/daemon.log; do
      [ -f "$log" ] && { echo "--- daemon.log (tail) ---" >&2; tail -20 "$log" >&2; }
    done
  fi
  acy stop >/dev/null 2>&1 || true
  local pid
  pid="$(daemon_pid)"
  [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null || true
  rm -rf "$WORK"
}
trap teardown EXIT

setup_repo() {
  mkdir -p "$R/src" "$R/.acyclic" "$STORES"
  printf 'store_dir = "%s"\n' "$STORES" > "$R/.acyclic/config.toml"
  printf 'ORIGINAL\n' > "$R/src/main.rs"
  printf 'SECRET=1\n' > "$R/.env"
  printf '.env\ngenerated.bin\n' > "$R/.gitignore"
}

# The daemon captures asynchronously; give a queued checkpoint time to land.
settle() {
  sleep "${1:-1}"
}

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

acy() {
  "$BIN" --repo "$R" "$@"
}

daemon_pid() {
  cat "$STORES"/*/daemon.pid 2>/dev/null || true
}

teardown() {
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

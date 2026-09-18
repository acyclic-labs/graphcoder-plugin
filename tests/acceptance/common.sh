#!/usr/bin/env bash
# Shared setup for acceptance scripts. Each script gets an isolated repo and
# store under a temp root, and a daemon that is always torn down on exit.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${ACYCLIC_BIN:-$REPO_ROOT/target/debug/acyclic}"
SDK_ROOT="$(cd "$REPO_ROOT/../../../sdk" && pwd)"
QUAL="${ACYCLIC_QUAL:-$SDK_ROOT/target/debug/qualify}"

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
  local code=$?
  # A reader that stops early (`| head -1`, `| awk '{...; exit}'`, `| grep -q`)
  # closes the pipe while the client is still writing; the client dies of
  # SIGPIPE (141) and pipefail would call the whole pipeline a failure.
  # That is the reader's choice, not a client error.
  [ "$code" -eq 141 ] && return 0
  return "$code"
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

# Portable bound on a live host CLI session: stock macOS ships neither GNU
# `timeout` nor `gtimeout` (coreutils), so scripts that shell out to a real
# agent CLI (codex-e2e.sh, cursor-e2e.sh) cannot rely on either being
# present. Runs "$@" as its own process group (`set -m`) and signals the
# whole group, not just the direct child, if it outlives $1 seconds — a
# plain `kill $pid` only reaches the CLI's own process, and codex/cursor
# spawn subprocess trees for tool calls (shell commands the agent runs);
# without the group kill those grandchildren outlive the "timed out" test
# and can wedge the next script's daemon or repo. TERM first, KILL 2s
# later for anything that ignored it. Preserves "$@"'s exit code; stdout/
# stderr pass through untouched so the caller's own redirection and
# command substitution work exactly as with a real `timeout`.
with_timeout() {
  local secs="$1"
  shift
  local was_m
  case "$-" in *m*) was_m=1 ;; *) was_m=0 ;; esac
  set -m
  ("$@") &
  local pid=$!
  [ "$was_m" -eq 1 ] || set +m
  (
    sleep "$secs" 2>/dev/null
    kill -TERM "-$pid" 2>/dev/null
    sleep 2
    kill -KILL "-$pid" 2>/dev/null
  ) &
  local watchdog=$!
  local code=0
  wait "$pid" 2>/dev/null || code=$?
  kill "$watchdog" 2>/dev/null
  wait "$watchdog" 2>/dev/null
  return "$code"
}

# --- Model pinning for the live host-session tests -------------------------
# claude-e2e.sh, claude-merge-e2e.sh, codex-e2e.sh, cursor-e2e.sh and
# mcp-clients-e2e.sh all assert on model-driven behavior: that the agent
# calls the MCP tool instead of shelling out, that it performs the steps in
# the order given, that a checkpoint lands with the right attribution. The
# model is therefore a test INPUT, not an ambient setting. Left unpinned,
# these scripts run on whatever default the installed CLI happens to have,
# so a vendor moving that default silently changes what a paid session
# measures and a real regression is indistinguishable from a flake. Each
# host gets its own override so one can be re-pointed without disturbing
# the others; set a var empty to fall back to that CLI's own default.
#
# Where the pinned ids come from: each was RUN, not read off a list. Claude
# is pinned to claude-opus-5 because that is what this gate is green on —
# claude-e2e.sh passes on Opus 5 and fails on claude-sonnet-5, three runs
# out of three, with src/main.rs never written (the model does not complete
# step 1 of the three-step prompt). That is a real behavioral difference,
# not a bad id: a direct session confirms claude-sonnet-5 resolves and
# bills normally. Until the prompt is hardened enough for a mid-tier model
# to drive it, pinning to Sonnet would ship a red gate. Cursor comes from
# `cursor-agent models`, which marks composer-2.5 as current. Codex ships
# no offline way to enumerate valid ids (`codex exec --help` documents the
# flag but lists no values) and rejects a wrong one outright, so it stays
# on the CLI default until someone pins a value they have actually run.
E2E_CLAUDE_MODEL="${ACYCLIC_E2E_CLAUDE_MODEL-claude-opus-5}"
E2E_CURSOR_MODEL="${ACYCLIC_E2E_CURSOR_MODEL-composer-2.5}"
E2E_CODEX_MODEL="${ACYCLIC_E2E_CODEX_MODEL-}"

# `--model X` per host, or nothing when that host is unpinned. Stock macOS
# bash 3.2 treats "${ARR[@]}" on an EMPTY array as an unbound variable under
# `set -u`, so every expansion of these at a call site must use the
# ${ARR[@]+"${ARR[@]}"} guard, not a bare "${ARR[@]}".
CLAUDE_MODEL_ARGS=()
CURSOR_MODEL_ARGS=()
CODEX_MODEL_ARGS=()
if [ -n "$E2E_CLAUDE_MODEL" ]; then CLAUDE_MODEL_ARGS=(--model "$E2E_CLAUDE_MODEL"); fi
if [ -n "$E2E_CURSOR_MODEL" ]; then CURSOR_MODEL_ARGS=(--model "$E2E_CURSOR_MODEL"); fi
if [ -n "$E2E_CODEX_MODEL" ]; then CODEX_MODEL_ARGS=(--model "$E2E_CODEX_MODEL"); fi

# One line in the log naming the model a paid session actually ran on, so a
# stored transcript is still interpretable after the defaults move on.
e2e_model_note() {
  echo "  $1 model: ${2:-<CLI default, unpinned>}"
}

# Fails the calling script's skip() path (must be defined by the caller)
# with a clear reason when an installed host CLI's --help output doesn't
# mention a flag the test depends on, rather than letting a stale flag
# produce an opaque CLI error partway through a live session.
require_flag() {
  local help_output="$1" flag="$2" cli_name="$3"
  printf '%s' "$help_output" | grep -qe "$flag" \
    || skip "$cli_name CLI is missing expected flag $flag (version drift? check its --help)"
}

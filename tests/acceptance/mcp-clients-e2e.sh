#!/usr/bin/env bash
# The MCP adapter driven by every real MCP client CLI on this machine, not a
# scripted client (that is mcp-e2e.sh). For each of claude, codex,
# cursor-agent and opencode that is on PATH: register `acyclic mcp` the way
# that host expects, run one headless session that calls brief, checkpoint
# and timeline, and assert the checkpoint landed in the daemon. A host that
# is not installed is reported and skipped; nothing here is required.
#
# Needs the host CLIs and their credentials; skips (exit 0) when none is
# present unless ACYCLIC_E2E_REQUIRED=1. Costs one short model session per
# host. docs/manual-testing.md walks the same steps by hand.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
source "$REPO_ROOT/scripts/product.sh"
NAME="$PRODUCT_NAME"

skip() {
  if [ "${ACYCLIC_E2E_REQUIRED:-0}" = "1" ]; then
    fail "$*"
  fi
  echo "SKIP($(basename "$0")): $*"
  exit 0
}

setup_repo
acy init >/dev/null || fail "init"
ran=0

prompt_for() {
  printf 'Use only the %s MCP tools, no shell and no file edits. 1) call %s brief. 2) call %s checkpoint with message "%s". 3) call %s timeline. ' \
    "$NAME" "$NAME" "$NAME" "$1" "$NAME"
  printf 'Then reply with the exact checkpoint id the checkpoint tool returned and nothing else.'
}

# The checkpoint the model asked for is in the timeline with its message,
# attributed to nothing (MCP hosts have no session id to hand us).
assert_landed() {
  settle 1
  acy timeline --limit 20 | grep -q "$1" || fail "$2: checkpoint '$1' not in timeline: $(acy timeline --limit 20)"
  echo "ok: $2 called checkpoint through MCP ('$1')"
  ran=$((ran + 1))
}

# Claude Code: --mcp-config takes an mcpServers file (same shape as Claude
# Desktop's); --strict-mcp-config ignores the user's own servers so the
# session sees exactly one.
if command -v claude >/dev/null 2>&1; then
  printf '{"mcpServers":{"%s":{"command":"%s","args":["mcp","--repo","%s"]}}}\n' "$NAME" "$BIN" "$R" > "$WORK/claude-mcp.json"
  (cd "$R" && with_timeout 180 claude -p "$(prompt_for mcp-from-claude-code)" \
    --mcp-config "$WORK/claude-mcp.json" --strict-mcp-config \
    --allowedTools "mcp__${NAME}__brief,mcp__${NAME}__checkpoint,mcp__${NAME}__timeline" \
    --output-format json >"$WORK/claude.out" 2>"$WORK/claude.err") \
    || fail "claude session failed: $(tail -c 300 "$WORK/claude.err")"
  assert_landed mcp-from-claude-code claude
else
  echo "skip: claude CLI not on PATH"
fi

# Codex: MCP servers live in config.toml; -c overrides keep the user's file
# untouched. `default_tools_approval_mode = "approve"` is what lets a
# non-interactive `codex exec` (approval policy: never) call the tools at
# all — without it every call fails with "requires approval".
if command -v codex >/dev/null 2>&1; then
  (cd "$R" && with_timeout 180 codex exec "$(prompt_for mcp-from-codex)" \
    --skip-git-repo-check --json -o "$WORK/codex.last" \
    -c "mcp_servers.${NAME}.command=\"$BIN\"" \
    -c "mcp_servers.${NAME}.args=[\"mcp\",\"--repo\",\"$R\"]" \
    -c "mcp_servers.${NAME}.default_tools_approval_mode=\"approve\"" \
    >"$WORK/codex.out" 2>"$WORK/codex.err") \
    || fail "codex session failed: $(tail -c 300 "$WORK/codex.err")"
  assert_landed mcp-from-codex codex
else
  echo "skip: codex CLI not on PATH"
fi

# cursor-agent: reads the project's .cursor/mcp.json that `install cursor`
# writes, but only after the server is on its approved list (`mcp enable`,
# per machine); --approve-mcps and --force skip the interactive prompts.
if command -v cursor-agent >/dev/null 2>&1; then
  acy install cursor >/dev/null || fail "install cursor"
  (cd "$R" && cursor-agent mcp enable "$NAME" >/dev/null 2>&1) || fail "cursor-agent mcp enable"
  tools="$(cd "$R" && cursor-agent mcp list-tools "$NAME" 2>&1 || true)"
  printf '%s' "$tools" | grep -q "checkpoint" || fail "cursor-agent does not list the checkpoint tool: $tools"
  (cd "$R" && with_timeout 180 cursor-agent -p "$(prompt_for mcp-from-cursor-agent)" \
    --output-format json --approve-mcps --force --trust \
    >"$WORK/cursor.out" 2>"$WORK/cursor.err") \
    || fail "cursor-agent session failed: $(tail -c 300 "$WORK/cursor.err")"
  assert_landed mcp-from-cursor-agent cursor-agent
else
  echo "skip: cursor-agent CLI not on PATH"
fi

# OpenCode: project-scoped opencode.json, `mcp.<name>` with a command
# array. No adapter writes this yet (see TODO(more hosts) in install.rs);
# `opencode mcp list` performs the real initialize handshake, which is as
# far as this goes without a configured model.
if command -v opencode >/dev/null 2>&1; then
  printf '{"$schema":"https://opencode.ai/config.json","mcp":{"%s":{"type":"local","command":["%s","mcp","--repo","%s"],"enabled":true}}}\n' \
    "$NAME" "$BIN" "$R" > "$R/opencode.json"
  # Capture first: `| grep -q` would close the pipe early and pipefail
  # would report opencode's SIGPIPE as a failure.
  listing="$(cd "$R" && opencode mcp list 2>&1 || true)"
  printf '%s' "$listing" | grep -q "connected" || fail "opencode did not report $NAME connected: $listing"
  echo "ok: opencode connected to the server (handshake only)"
  ran=$((ran + 1))
else
  echo "skip: opencode CLI not on PATH"
fi

[ "$ran" -gt 0 ] || skip "no MCP client CLI installed"
pass "mcp-clients-e2e: $ran real MCP client(s) drove the server"

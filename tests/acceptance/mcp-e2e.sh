#!/usr/bin/env bash
# The MCP analogue of codex-e2e.sh / cursor-e2e.sh, for hosts with no
# lifecycle-hook API (Claude Desktop, VS Code, Cursor's MCP path): drives
# `acyclic mcp` over its real stdio transport with a scripted JSON-RPC
# client and checks the server against a live daemon end-to-end.
# Asserts: the initialize handshake completes and advertises tools; every
# tool the adapters document is listed; `checkpoint` lands a row the CLI
# sees under the same label; `timeline` reports it back over MCP; and the
# server exits cleanly when the host closes stdin.
#
# Needs only the built binary — no host app, credentials, or model session
# — so it runs on every CI pass, not behind ACYCLIC_E2E=1. What it cannot
# cover is the host app itself (config discovery, tool approval UI); the
# install-side merges are unit-tested in install.rs.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

setup_repo
acy init >/dev/null || fail "init"

IN="$WORK/mcp.in"
OUT="$WORK/mcp.out"
ERR="$WORK/mcp.stderr"
mkfifo "$IN"
"$BIN" --repo "$R" mcp <"$IN" >"$OUT" 2>"$ERR" &
MCP_PID=$!
# Hold the write end open so the server sees a live stdin between requests.
exec 3>"$IN"

send() {
  printf '%s\n' "$1" >&3
}

# One JSON-RPC response line per request id, in any order.
wait_for_id() {
  local id="$1" tries=0
  until grep -q "\"id\":$id," "$OUT" 2>/dev/null; do
    tries=$((tries + 1))
    if [ "$tries" -gt 150 ]; then
      fail "no response for request $id after 15s
--- stdout ---
$(tail -c 600 "$OUT")
--- stderr ---
$(tail -c 400 "$ERR")"
    fi
    sleep 0.1
  done
}

response() {
  grep "\"id\":$1," "$OUT" | head -1
}

send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"mcp-e2e","version":"0"}}}'
wait_for_id 1
response 1 | grep -q '"capabilities":{"tools"' || fail "initialize did not advertise tools: $(response 1)"
response 1 | grep -q '"instructions":' || fail "initialize carried no instructions: $(response 1)"
send '{"jsonrpc":"2.0","method":"notifications/initialized"}'

send '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
wait_for_id 2
for tool in checkpoint timeline turns rewind diff restore brief; do
  response 2 | grep -q "\"name\":\"$tool\"" || fail "tools/list is missing $tool: $(response 2)"
done

LABEL="mcp e2e $$"
send "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"checkpoint\",\"arguments\":{\"message\":\"$LABEL\"}}}"
wait_for_id 3
response 3 | grep -q '"isError":false' || fail "checkpoint tool errored: $(response 3)"
response 3 | grep -q 'checkpoint #' || fail "checkpoint tool returned no id: $(response 3)"

send '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"timeline","arguments":{}}}'
wait_for_id 4
response 4 | grep -q "$LABEL" || fail "timeline over MCP does not show the checkpoint: $(response 4)"

# The CLI and the MCP server share one daemon and one index: the row the
# tool created must be the same row the CLI lists.
settle
acy timeline | grep -q "$LABEL" || fail "CLI timeline does not show the MCP checkpoint: $(acy timeline)"

# A host stops its MCP server by closing stdin; the process must exit on
# its own rather than needing a kill.
exec 3>&-
tries=0
while kill -0 "$MCP_PID" 2>/dev/null; do
  tries=$((tries + 1))
  if [ "$tries" -gt 50 ]; then
    kill -9 "$MCP_PID" 2>/dev/null || true
    fail "mcp server still running 5s after stdin closed"
  fi
  sleep 0.1
done
wait "$MCP_PID" 2>/dev/null || fail "mcp server exited non-zero: $(tail -c 400 "$ERR")"

pass "handshake, 7 tools listed, checkpoint + timeline round-trip through the shared daemon, clean exit on stdin close"

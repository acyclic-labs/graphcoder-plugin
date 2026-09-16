#!/usr/bin/env bash
# The MCP analogue of codex-e2e.sh / cursor-e2e.sh, for hosts with no
# lifecycle-hook API (Claude Desktop, VS Code, Cursor's MCP path): drives
# `acyclic mcp` over its real stdio transport with a scripted JSON-RPC
# client and checks the server against a live daemon end-to-end.
# Asserts: the initialize handshake completes and advertises tools; every
# tool the adapters document is listed and called for real — `checkpoint`
# lands a row the CLI sees under the same label, `timeline` reports it
# back, `diff` names an edit, `restore` brings one path back (and refuses
# an empty list), `rewind` refuses without confirm=true and restores the
# tree with it, `brief`, `turns` and `summary` answer — and the server exits cleanly
# when the host closes stdin.
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

# The response line for request id $1, or nothing yet. Tolerates the id as
# any key, with or without a space after the colon, and quoted (a string
# id echoed back) — the server writes compact `"id":N,` today, but a
# serializer change should show up as an assertion, not a timeout here.
response() {
  grep -E "\"id\": ?\"?$1\"?[,} ]" "$OUT" | head -1
}

# Responses arrive in any order; poll until $1's line is there.
wait_for_id() {
  local id="$1" tries=0
  until [ -n "$(response "$id")" ]; do
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

send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"mcp-e2e","version":"0"}}}'
wait_for_id 1
init="$(response 1)"
[[ "$init" == *'"capabilities":{"tools"'* ]] || fail "initialize did not advertise tools: $init"
[[ "$init" == *'"instructions":'* ]] || fail "initialize carried no instructions: $init"
send '{"jsonrpc":"2.0","method":"notifications/initialized"}'

send '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
wait_for_id 2
tools="$(response 2)"
for tool in checkpoint timeline turns rewind diff restore brief summary; do
  [[ "$tools" == *"\"name\":\"$tool\""* ]] || fail "tools/list is missing $tool: $tools"
done

LABEL="mcp e2e $$"
send "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"checkpoint\",\"arguments\":{\"message\":\"$LABEL\"}}}"
wait_for_id 3
checkpoint="$(response 3)"
[[ "$checkpoint" == *'"isError":false'* ]] || fail "checkpoint tool errored: $checkpoint"
[[ "$checkpoint" == *'checkpoint #'* ]] || fail "checkpoint tool returned no id: $checkpoint"

send '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"timeline","arguments":{}}}'
wait_for_id 4
timeline="$(response 4)"
[[ "$timeline" == *"$LABEL"* ]] || fail "timeline over MCP does not show the checkpoint: $timeline"

# The CLI and the MCP server share one daemon and one index: the row the
# tool created must be the same row the CLI lists.
settle
acy timeline | grep -q "$LABEL" || fail "CLI timeline does not show the MCP checkpoint: $(acy timeline)"
BASE_ID="$(printf '%s' "$checkpoint" | grep -oE 'checkpoint #[0-9]+' | head -1 | tr -dc '0-9' || true)"
[ -n "$BASE_ID" ] || fail "could not parse the checkpoint id from: $checkpoint"

# The mutating tools, against a real edit: change a file, checkpoint it,
# then bring it back with a single-path restore, see the change in diff,
# and rewind the whole tree (refused without confirm=true).
printf 'CHANGED\n' > "$R/src/main.rs"
send '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"checkpoint","arguments":{"message":"after edit"}}}'
wait_for_id 5
[[ "$(response 5)" == *'"isError":false'* ]] || fail "second checkpoint errored: $(response 5)"

send "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"tools/call\",\"params\":{\"name\":\"diff\",\"arguments\":{\"before\":$BASE_ID}}}"
wait_for_id 6
[[ "$(response 6)" == *'modified src/main.rs'* ]] || fail "diff over MCP does not name the edit: $(response 6)"

send "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{\"name\":\"restore\",\"arguments\":{\"checkpoint\":$BASE_ID,\"paths\":[\"src/main.rs\"]}}}"
wait_for_id 7
restore="$(response 7)"
[[ "$restore" == *'"isError":false'* ]] || fail "restore errored: $restore"
[[ "$restore" == *'recorded as #'* ]] || fail "restore did not report its recorded checkpoint: $restore"
[ "$(cat "$R/src/main.rs")" = "ORIGINAL" ] || fail "restore did not bring src/main.rs back: $(cat "$R/src/main.rs")"

send "{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"tools/call\",\"params\":{\"name\":\"restore\",\"arguments\":{\"checkpoint\":$BASE_ID,\"paths\":[]}}}"
wait_for_id 8
[[ "$(response 8)" == *'at least one'* ]] || fail "empty restore paths were not refused: $(response 8)"

printf 'CHANGED AGAIN\n' > "$R/src/main.rs"
send "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"tools/call\",\"params\":{\"name\":\"rewind\",\"arguments\":{\"checkpoint\":$BASE_ID}}}"
wait_for_id 9
[[ "$(response 9)" == *'confirm=true'* ]] || fail "rewind without confirm was not refused: $(response 9)"
[ "$(cat "$R/src/main.rs")" = "CHANGED AGAIN" ] || fail "refused rewind still touched the tree"

send "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"rewind\",\"arguments\":{\"checkpoint\":$BASE_ID,\"confirm\":true}}}"
wait_for_id 10
[[ "$(response 10)" == *"restored checkpoint #$BASE_ID"* ]] || fail "rewind over MCP failed: $(response 10)"
[ "$(cat "$R/src/main.rs")" = "ORIGINAL" ] || fail "rewind did not restore src/main.rs: $(cat "$R/src/main.rs")"

send '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"brief","arguments":{}}}'
wait_for_id 11
[[ "$(response 11)" == *'"isError":false'* ]] || fail "brief errored: $(response 11)"
[[ "$(response 11)" != *'{NAME}'* ]] || fail "brief leaked an unrendered product-name placeholder: $(response 11)"

send '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"turns","arguments":{}}}'
wait_for_id 12
[[ "$(response 12)" == *'"isError":false'* ]] || fail "turns errored: $(response 12)"

# Speculation is off here, so `summary` must answer that plainly rather
# than erroring — and must not produce one on demand, which would spend the
# developer's money because a tool was called.
send '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"summary","arguments":{}}}'
wait_for_id 13
summary="$(response 13)"
[[ "$summary" == *'"isError":false'* ]] || fail "summary errored with speculation off: $summary"
[[ "$summary" == *'no summary'* ]] || fail "summary should report none is available: $summary"

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

pass "handshake, 8 tools listed and every one called (checkpoint, timeline, diff, restore, rewind with its confirm gate, brief, turns, summary) through the shared daemon, clean exit on stdin close"

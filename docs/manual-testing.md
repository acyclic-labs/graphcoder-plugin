# Manual testing against real hosts

CI proves the engine, the hook entrypoint and the MCP server with scripted
clients. It cannot prove that a given release of Claude Code, Codex, Cursor,
OpenCode, Claude Desktop or VS Code still loads our config and calls us:
those CLIs move fast (Codex renamed `session_id` to `thread_id` and changed
its `hooks.json` shape within a few releases). This page is the checklist
for running the plugin against the real hosts on your machine, what each
step should show, and the results of the last pass.

Everything below runs in a **scratch repo**, never in a real project: a
first `init` baselines the whole tree, and a repo with a multi-GiB `target/`
or `node_modules/` takes minutes and disk before exclusions apply.

## Setup

```sh
cargo build --release -p acyclic
export ACYCLIC_BIN="$PWD/target/release/acyclic"

mkdir -p /tmp/acyclic-smoke/src && cd /tmp/acyclic-smoke
git init -q && echo ORIGINAL > src/main.rs && git add -A && git commit -qm init
$ACYCLIC_BIN init          # "daemon ready — checkpointing is on"
$ACYCLIC_BIN timeline      # one row: #1 baseline
```

Every hook host below invokes bare `acyclic` from the session's cwd, so put
the binary under test first on `PATH` (`export PATH="$(dirname
"$ACYCLIC_BIN"):$PATH"`) or the session will use whatever `acyclic` was
installed globally.

## Scripted version of this page

The live suites do the hook-host checks unattended and are the fastest way
to re-verify after a host upgrade:

```sh
export ACYCLIC_E2E=1 ACYCLIC_E2E_REQUIRED=1 ACYCLIC_BIN   # ACYCLIC_BIN from Setup above
tests/acceptance/claude-e2e.sh        # Claude Code hooks
tests/acceptance/codex-e2e.sh         # Codex hooks
tests/acceptance/cursor-e2e.sh        # cursor-agent hooks
tests/acceptance/mcp-clients-e2e.sh   # MCP server driven by claude, codex, cursor-agent, opencode
```

Each one installs the adapter into a throwaway repo, runs one short model
session, and asserts that hooks fired with session attribution, the edits
were captured, `diff` names them and `rewind --session-start` restores the
tree. `ACYCLIC_E2E_REQUIRED=1` turns "CLI not installed" from a skip into a
failure, which is what you want when you are specifically checking a host.

## Hook hosts (automatic checkpoints)

### Claude Code

```sh
acyclic install claude-code
claude                                    # interactive; or: claude -p "<prompt>" --settings .claude/settings.json
```

In the session, ask it to overwrite `src/main.rs` and run a shell command.
Then in another terminal:

```sh
acyclic timeline           # pre/post rows attributed to the tool (Write, Bash) and the session
acyclic sessions           # the session id Claude reported
acyclic diff <first> <last>
acyclic rewind --session-start <session> --yes
cat src/main.rs            # ORIGINAL
```

`/rewind`, `/timeline` and `/fork` are also available inside the session.

### Codex

```sh
acyclic install codex
codex                      # first run: `/hooks` in the TUI, trust the project hooks once
```

Codex runs project hooks only after you trust them, and only from the
`{"hooks": {...}}` shape in `.codex/hooks.json`. If the timeline stays at
`baseline` after a session, check both: `cat .codex/hooks.json` should show
the events under a top-level `hooks` key (re-run `install codex` to migrate
an older flat file), and `/hooks` should list them as trusted. Headless:
`codex exec "<prompt>" --json` prints `thread_id`; that is the value the
hooks report as `session_id`, so `acyclic timeline --session <thread_id>`
filters to that run.

### Cursor (cursor-agent and the desktop app)

```sh
acyclic install cursor
cursor-agent               # or open the repo in Cursor.app
```

Cursor asks to trust the workspace before loading `.cursor/hooks.json`.
The checks are the same as for Claude Code. `install cursor` also writes
`.cursor/mcp.json`; the MCP section below covers approving it.

## MCP hosts (tools the model calls)

The MCP adapter exposes `brief`, `checkpoint`, `timeline`, `rewind`,
`diff`, `restore` and `turns`. There are no hooks, so the only automatic
checkpoints come from the idle timer (`auto_checkpoint_idle_ms`, 5 s by
default). The check for every MCP host is the same prompt:

> Use only the acyclic MCP tools. Call `brief`, then `checkpoint` with
> message "mcp-from-<host>", then `timeline`. Reply with the checkpoint id.

and then `acyclic timeline` in a terminal must show a row carrying that
message. A `noop` kind on that row is expected when nothing changed since
the last snapshot: the tool call still went through.

### Claude Code as an MCP client

Useful because it is scriptable; Claude Desktop reads the same file shape.

```sh
cat > /tmp/acyclic-mcp.json <<EOF
{"mcpServers":{"acyclic":{"command":"$ACYCLIC_BIN","args":["mcp","--repo","$PWD"]}}}
EOF
claude -p "<the prompt above>" --mcp-config /tmp/acyclic-mcp.json --strict-mcp-config \
  --allowedTools mcp__acyclic__brief,mcp__acyclic__checkpoint,mcp__acyclic__timeline
```

### Codex as an MCP client

Codex keeps MCP servers in `~/.codex/config.toml` under `[mcp_servers.<name>]`
(TOML, so `acyclic install` does not write it yet). For a one-off test pass
the entry as overrides, and set the per-server approval mode: a headless
`codex exec` auto-rejects approval prompts, and without it every tool call
fails with "MCP tool call requires approval".

```sh
codex exec "<the prompt above>" --json \
  -c "mcp_servers.acyclic.command=\"$ACYCLIC_BIN\"" \
  -c "mcp_servers.acyclic.args=[\"mcp\",\"--repo\",\"$PWD\"]" \
  -c 'mcp_servers.acyclic.default_tools_approval_mode="approve"'
```

To make it permanent: `codex mcp add acyclic -- $ACYCLIC_BIN mcp --repo $PWD`,
then add `default_tools_approval_mode = "approve"` under that table. The
same file is read by the ChatGPT desktop app and the Codex IDE extension.

### cursor-agent as an MCP client

```sh
acyclic install cursor                  # writes .cursor/mcp.json
cursor-agent mcp enable acyclic         # per-machine approval, once
cursor-agent mcp list-tools acyclic     # the seven tools
cursor-agent -p "<the prompt above>" --approve-mcps --force --trust
```

Cursor.app reads the same `.cursor/mcp.json`; approve the server in
Settings → MCP when prompted.

### OpenCode

No adapter yet. A project-scoped `opencode.json` works:

```sh
cat > opencode.json <<EOF
{"\$schema":"https://opencode.ai/config.json",
 "mcp":{"acyclic":{"type":"local","command":["$ACYCLIC_BIN","mcp","--repo","$PWD"],"enabled":true}}}
EOF
opencode mcp list                       # "✓ acyclic connected"
opencode run "<the prompt above>"       # needs a configured model/provider
```

### Claude Desktop

```sh
acyclic install claude-desktop          # adds mcpServers.acyclic-<repo name> to the global config
```

Quit and relaunch Claude Desktop. `~/Library/Logs/Claude/mcp-server-acyclic-<repo name>.log`
(macOS) should show `Server started and connected successfully`, then
`initialize` and `tools/list` exchanges. In a new chat, send the prompt above
and confirm the checkpoint in a terminal. The entry is per machine and binds
one repo under its own key; re-run the install in another repo to add a second entry, and remove the entry from
`claude_desktop_config.json` when the scratch repo is gone.

### VS Code (Copilot agent mode)

```sh
acyclic install vscode                  # writes .vscode/mcp.json
code .
```

Open the Chat view in agent mode, start the `acyclic` server from the MCP
servers list (or accept the prompt), send the prompt above, check the
terminal. Needs the GitHub Copilot Chat extension.

## Cleanup

```sh
acyclic stop                            # this repo's daemon
rm -rf ~/.local/share/acyclic/stores/<hash>   # the scratch store; `acyclic status` prints the path
```

`acyclic install claude-desktop` and `cursor-agent mcp enable` change
per-machine state; undo them by hand if the scratch repo goes away.

## Last full pass

2026-09-14, macOS 26.5, release build of this branch.

| Host | Version | Path | Result |
|---|---|---|---|
| Claude Code | 2.1.270 | hooks (`claude-e2e.sh`) | pass: pre/post rows, Write + Bash attribution, diff, rewind |
| Claude Code | 2.1.270 | MCP client | pass: `brief`, `checkpoint`, `timeline` called, checkpoint landed |
| Codex | 0.154.0 | hooks (`codex-e2e.sh`) | pass after the fixes below: pre/post rows with turn attribution, Bash attribution, the edit proven by `diff`, rewind |
| Codex | 0.154.0 | MCP client | pass with `default_tools_approval_mode = "approve"`; fails without it |
| cursor-agent | 2026.09.10 | hooks (`cursor-e2e.sh`) | pass |
| cursor-agent | 2026.09.10 | MCP client | pass after `mcp enable`; seven tools listed, checkpoint landed |
| OpenCode | 1.18.10 | MCP | connected (handshake); no model configured for a tool call |
| Claude Desktop | current | MCP | server started, `initialize` + `tools/list` succeeded on launch; in-chat tool call not exercised |
| VS Code | 1.137.0 | MCP | config written; Copilot extension not installed here, not exercised |

Findings fixed during that pass: Codex 0.154 ignores a `hooks.json` without
the top-level `hooks` key (the adapter wrote the flat shape; now migrated on
re-install); Codex's edit tool is `apply_patch`, which the hook matcher did
not name (the suite does not require an `apply_patch` row, since a failed
patch makes the model fall back to a shell redirect; the `diff` check is
what proves the edit was captured); Codex's JSONL emits `thread_id` rather than `session_id`; and the
`brief` tool returned a literal `{NAME}` when no session was on record.

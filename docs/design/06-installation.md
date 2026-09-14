# Installation & usage

One engine, thin adapters. Every capability lives in a single local engine — the `acyclic` CLI and its background daemon (watcher, snapshot store, index). Each coding agent gets a thin adapter wiring the engine into that host's native extension points. The engine is host-agnostic; only the adapter knows it's running inside Claude Code, Codex, or OpenCode. Any agent that can run shell commands can use the engine with no adapter at all.

## Install flow

```sh
curl -fsSL https://acyclic.dev/install.sh | sh   # or: brew install acyclic
acyclic init                                     # in your repo: starts the daemon, builds the first snapshot
acyclic install claude-code                      # or: codex · cursor · opencode · --agents-md · claude-desktop · vscode
```

`acyclic install` detects the host and configures its adapter. From then on, the dev starts their agent as usual — checkpointing is on.

## Per-host adapters

| Host | Mechanism |
|---|---|
| **Claude Code** | Native plugin: PreToolUse/PostToolUse hooks trigger checkpoints around edits and commands; slash commands (`/rewind`, `/fork`, `/timeline`) surface the engine to the dev; a skill teaches the agent the engine's verbs (self-rollback, fork-and-try, blast-radius diff). |
| **Codex** | `.codex/hooks.json` lifecycle hooks (`PreToolUse`/`PostToolUse`/`UserPromptSubmit`/`SessionStart`/`SessionEnd`) drive the same checkpointing as Claude Code — Codex's payload shape matches closely enough that `acyclic hook` needs no host-specific parsing; engine verbs are taught via the same AGENTS.md block `--agents-md` writes. |
| **Cursor** | `.cursor/hooks.json` agent hooks (`beforeShellExecution`/`afterShellExecution`/`afterFileEdit`/`beforeSubmitPrompt`/`sessionStart`/`sessionEnd`); Cursor's payload uses `conversation_id` and a bare `command` string rather than Claude/Codex's `session_id`/`tool_name`, so `acyclic hook` falls back to those fields when present. An always-applied `.cursor/rules/<name>.mdc` (product-named, see `product.toml`) teaches the engine's verbs. Also registers `acyclic mcp` at a project-scoped `.cursor/mcp.json` (Cursor speaks MCP directly, same `mcpServers` shape as Claude Desktop below) — hooks give automatic checkpointing, MCP additionally gives named tools the model can call explicitly. |
| **OpenCode** | Plugin using its hook and command systems; same shape as the Claude Code adapter. Not started. |
| **Anything else** | `acyclic install --agents-md` drops an instructions block teaching any shell-capable agent the CLI. Degraded gracefully: no hook-triggered checkpoints, but watcher-driven ones still work. |
| **Claude Desktop** | No lifecycle-hook API exists, so there is nothing to hook into — `acyclic install claude-desktop` instead registers `acyclic mcp` (an MCP stdio server) as an `mcpServers` entry in the user's global `claude_desktop_config.json`; per-machine and per-repo, not checked into the repo. The server exposes `checkpoint`/`timeline`/`rewind`/`diff`/`restore`/`turns`/`brief` as MCP tools, each translating directly into the engine's existing wire protocol. Checkpointing is not automatic on every tool call the way it is for a hooked host: it happens when the model calls `checkpoint`, or via the engine's `auto_checkpoint_idle_ms` idle timer once the watcher has pending changes — a real capability gap, not hidden by the tool descriptions that steer the model toward calling it. Shipped after the evaluation below; `.mcpb` packaging and a possibly-shared server are the next things to revisit, not blockers on today's flow. |
| **VS Code** (Copilot agent mode) | Same shape as Claude Desktop — no lifecycle-hook API, MCP is the only extension point — but VS Code supports a project-scoped `.vscode/mcp.json`, checked in like the hook-based adapters rather than global-only. Different JSON shape from Claude Desktop/Cursor: top-level key is `servers` (not `mcpServers`), and every entry needs an explicit `"type": "stdio"`. Schema confirmed against VS Code's current docs and unit-tested; the server side is exercised by `tests/acceptance/mcp-e2e.sh` on every CI run; VS Code itself reading the file is not yet exercised — see the `TODO(verify)` on `vscode()` in `install.rs`. |

### More hosts (open TODO)

The two adapter shapes above — lifecycle hooks, and JSON-based MCP registration via `merge_mcp_server_json` — generalize to most other coding-agent CLIs and desktop apps, not just the five/six covered so far. See the `TODO(more hosts)` block directly above `adapters()` in `crates/acyclic/src/install.rs` for the process to follow and the concrete next candidates: **Kimi Code CLI** (has both TOML-based lifecycle hooks and MCP support — needs its own config-file/schema check, not yet done), **Windsurf**, **Zed**, **JetBrains AI assistants**, **Gemini CLI**, **Amazon Q Developer** — all unresearched as of this writing, likely MCP-capable but with unverified config shapes. **Codex's own MCP path** is a separate open item: its config is TOML (`[mcp_servers.<name>]` in `config.toml`), not JSON, so it needs its own writer rather than reusing `merge_mcp_server_json` — see the `TODO(desktop/IDE parity)` on `codex()`, which also flags that Codex's hooks may already cover its desktop app and IDE extension for free, since OpenAI's docs say they share the same config file.

## Configuration

- Per-repo: `.acyclic/config.toml` — checked in, so teams share policy: checkpoint granularity, guarded paths, dry-run default, store size caps, retention TTLs.
- Per-machine: `~/.config/acyclic/` — defaults.
- Zero config is a supported state — defaults are safe everywhere.

## Verb set

The same verbs everywhere, used by both the dev and the agent:

```
acyclic checkpoint [-m msg]      # snapshot now (hooks do this automatically)
acyclic timeline                 # checkpoints, linked to conversation turns
acyclic turns                    # which prompt caused which checkpoints   (Launch 2)
acyclic show <checkpoint>        # session, turn, prompt of one checkpoint  (Launch 2)
acyclic brief                    # previous session's end state + abandoned branches (Launch 2)
acyclic rewind <checkpoint>      # restore the tree, untracked files included
acyclic restore <checkpoint> <p> # restore one path, leave the rest        (Launch 2)
acyclic diff [<a> <b>|--turn N]  # blast radius since session start, between two points, or of one turn
acyclic fork [-n N]              # N copy-on-write working trees        (Launch 3)
acyclic promote <fork>           # merge the winning fork back          (Launch 3)
acyclic search <query>           # indexed search, checkpoint-aware     (Launch 5)
acyclic purge <pattern>          # remove content from ALL checkpoints  (compliance)
```

In hosts with an adapter these surface natively — `/rewind` in Claude Code rather than a shell command — and the agent reaches them as tools, so "try that again a different way" becomes a rollback plus a fresh attempt without the dev naming a checkpoint.

## Claude Desktop: ship decision

The release blocker on the Claude Desktop adapter (above) asked three questions before promoting it from "built, testable" to "documented, supported." Answered:

1. **Is there a non-MCP local-tool mechanism for Claude Desktop?** No. As of the current MCP spec (2026-07-28) and Anthropic's own Desktop Extensions docs, MCP — local (stdio) servers, optionally packaged as a one-click `.mcpb` bundle — is the only third-party extension point Desktop exposes for local tools; there is no separate lifecycle-hook API comparable to Claude Code's, and none is announced. This was true when the plan was drafted and is still true now.
2. **Can one server serve every repo, instead of one registration per repo?** Not with the current design: `acyclic mcp --repo <path>` binds one server process to one repo root at registration time (Desktop starts the subprocess with fixed `args`), so a dev working across N repos needs N `acyclic install claude-desktop` runs and N `mcpServers` entries. A single global server that resolves "which repo" from conversation context isn't possible today — MCP tool calls carry no notion of "the workspace the user has open" the way an IDE extension would. This is real friction versus the CLI adapters (one `acyclic install` per repo, but that's a one-time file the team already checks in) and versus IDE-integrated hosts. Accepted for v1: most users work from a small number of repos, and re-running one install command per repo is annoying, not broken.
3. **What does `.mcpb` packaging buy over the hand-merged config?** Removes the need to hand-edit `claude_desktop_config.json` (Desktop's installer merges it), and is Anthropic's own supported distribution format — but doesn't change the per-repo registration friction from (2), and adds a build/sign step to the release pipeline. Worth doing before broad distribution; not worth blocking on for the current per-machine, per-repo `acyclic install claude-desktop` flow this plan ships.

**Decision: ship the MCP adapter as designed**, with the per-repo registration friction called out in the README/install docs (already done, above) rather than hidden. Revisit `.mcpb` packaging and a possibly-shared server before actively promoting Claude Desktop as a first-class, equally-easy host alongside Claude Code/Codex/Cursor.

## Design commitment

This resolves the mechanism question in favor of **CLI-as-core with host adapters** (not MCP-as-core, not per-host deep builds). That choice is what makes OpenCode and future hosts nearly free. MCP wraps the CLI where a host has no other extension point — Claude Desktop's `acyclic mcp` adapter is exactly that: every MCP tool is a thin translation into the same `acyclic-proto::Op` the CLI and hooks already send the daemon, no engine logic lives in the MCP layer itself. See the Claude Desktop row above for why it's marked experimental rather than promoted to a supported host yet.

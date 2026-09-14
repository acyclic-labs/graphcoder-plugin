# Installation & usage

One engine, thin adapters. Every capability lives in a single local engine — the `acyclic` CLI and its background daemon (watcher, snapshot store, index). Each coding agent gets a thin adapter wiring the engine into that host's native extension points. The engine is host-agnostic; only the adapter knows it's running inside Claude Code, Codex, or OpenCode. Any agent that can run shell commands can use the engine with no adapter at all.

## Install flow

```sh
curl -fsSL https://acyclic.dev/install.sh | sh   # or: brew install acyclic
acyclic init                                     # in your repo: starts the daemon, builds the first snapshot
acyclic install claude-code                      # or: codex · cursor · opencode · --agents-md
```

`acyclic install` detects the host and configures its adapter. From then on, the dev starts their agent as usual — checkpointing is on.

## Per-host adapters

| Host | Mechanism |
|---|---|
| **Claude Code** | Native plugin: PreToolUse/PostToolUse hooks trigger checkpoints around edits and commands; slash commands (`/rewind`, `/fork`, `/timeline`) surface the engine to the dev; a skill teaches the agent the engine's verbs (self-rollback, fork-and-try, blast-radius diff). |
| **Codex** | `.codex/hooks.json` lifecycle hooks (`PreToolUse`/`PostToolUse`/`UserPromptSubmit`/`SessionStart`/`SessionEnd`) drive the same checkpointing as Claude Code — Codex's payload shape matches closely enough that `acyclic hook` needs no host-specific parsing; engine verbs are taught via the same AGENTS.md block `--agents-md` writes. |
| **Cursor** | `.cursor/hooks.json` agent hooks (`beforeShellExecution`/`afterShellExecution`/`afterFileEdit`/`beforeSubmitPrompt`/`sessionStart`/`sessionEnd`); Cursor's payload uses `conversation_id` and a bare `command` string rather than Claude/Codex's `session_id`/`tool_name`, so `acyclic hook` falls back to those fields when present. An always-applied `.cursor/rules/<name>.mdc` (product-named, see `product.toml`) teaches the engine's verbs. |
| **OpenCode** | Plugin using its hook and command systems; same shape as the Claude Code adapter. Not started. |
| **Anything else** | `acyclic install --agents-md` drops an instructions block teaching any shell-capable agent the CLI. Degraded gracefully: no hook-triggered checkpoints, but watcher-driven ones still work. |

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

## Design commitment

This resolves the mechanism question in favor of **CLI-as-core with host adapters** (not MCP-as-core, not per-host deep builds). That choice is what makes OpenCode and future hosts nearly free. MCP can wrap the CLI later where a host prefers it.

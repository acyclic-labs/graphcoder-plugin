# Launch 1 — Rewind

The snapshot store. Story: **"never fear letting the agent loose."**

Status: **built; release gate met.** Claims are tagged *shipped*, *not built*,
or *diverged* where what exists differs from what was planned.

## Features

1. **Auto-checkpoint every agent action** — *shipped, with a narrower trigger
   than "every action".* Hooks fire on a fixed mutating-tool matcher —
   `Edit`, `Write`, `MultiEdit`, `NotebookEdit`, `Bash`, `apply_patch`.
   Anything outside that list is uncheckpointed until the idle timer fires.
2. **Rewind for the dev** — *shipped*, including untracked, gitignored and
   `bash`-written state.
3. **Agent self-rollback** — *shipped*, as a skill plus a slash command.
4. **Blast-radius diff** — *shipped*, with one exclusion worth knowing:
   `.git` internals are captured but stripped from every diff.

## User journey

As written, and accurate — with one caveat at step 3: a rewind swaps the repo
directory inode, so an open shell or editor holds a stale one. The installed
skill tells the agent to `cd "$PWD"` afterwards; a human has to know to do the
same.

1. Maya runs `acyclic init` and starts her agent as usual. Checkpointing is on.
2. She asks for a risky change; the agent edits 30 files, regenerates a client,
   runs a migration script.
3. Tests fail structurally. `/rewind` puts the tree back exactly, generated
   client and gitignored artifacts included.
4. Mid-task the agent rolls *itself* back two checkpoints and tries a third
   route without poisoning the tree.
5. Before committing she opens the blast-radius diff and reviews it like a PR.

## What was built

- **Local content-addressed snapshot store** — *shipped, but not in this
  repo.* The store, Merkle DAG, content-defined chunking and the filesystem
  watcher all live in the `acyclic-fs` SDK, pinned by git revision. This
  repo builds the capture pipeline, checkpoint index, rewind, diff and the
  host adapters on top. The design docs never named that dependency, and it
  is the reason several items below are blocked rather than backlogged.
- **Store location** — *decided.* Never inside the repo. `.acyclic/` holds
  `config.toml` and nothing else; the store lives under
  `~/.local/share/acyclic/stores`. The original "in `.acyclic/` or a global
  per-machine store" is settled in favour of the latter.
- **Merkle-DAG tree representation, incremental capture** — *shipped,
  upstream.* No chunking code exists in this repo.
- **Host-tool hook integration** — *shipped*, and broader than planned:
  hooks for Claude Code, Codex and Cursor, plus an MCP adapter for hosts with
  no hook API at all.
- **Restore engine** — *shipped.* Atomic directory swap via
  `RENAME_EXCHANGE`/`RENAME_SWAP`.
- **Diff engine + surfaces** — *shipped*, CLI, slash command and MCP tool.
- **Retention/GC policy** — **not built, and blocked upstream.** The store is
  never garbage-collected, on purpose; see `07-compliance.md`. The only
  reclamation is `trash_ttl_days`, which prunes rewound-away trees. There is
  no checkpoint pruning and no store size cap.
- **Compliance-driven (design-in now, cannot retrofit)** — *partially.*
  Snapshot exclusions shipped. **Purge-through-history and encryption at rest
  do not exist**, and the "cannot retrofit" warning was correct: Launch 1
  shipped without chunk-level tombstoning.

### Decisions that were never written down

- **checkpoint() is not commit().** A checkpoint is a fast unpublished
  snapshot; publishing to the durable authority happens on a counter and an
  idle timer (`commit_every`, `commit_idle_ms`).
- **mtimes are never restored** on rewind, which the acceptance criterion
  below calls "byte-identical".
- **The hook never blocks and always exits 0.** `PreToolUse` waits a bounded
  2s and then lets the tool proceed *uncheckpointed* — a silent capability
  degradation, chosen so the engine can never wedge an agent.
- **Excluded paths are carried across the rewind swap**, so the live copy
  survives. For an excluded path the working tree is the only copy.
- **An idle timer** (`auto_checkpoint_idle_ms`) is the safety net for hosts
  with no hooks.

## Differentiation vs. native host checkpoints

Claude Code's native rewind tracks the agent's own file edits per turn. Ours
leads with what it can't do:

- Captures **what Bash did**: package installs, migration scripts, generators.
- Covers untracked and gitignored files.
- Survives across sessions.
- Identical behaviour in Codex, Cursor, OpenCode, and any shell-capable agent.
- Checkpoints are fork-able (Launch 3) and searchable (Launch 5).

## Acceptance criteria

- Checkpoint latency < 100ms p95 on a 1GB tree; < 1s p95 on 10GB.
  **Partially verified.** The gate runs 20k files / 256 MB, not 1GB, and
  measures the enqueue-acknowledgement branch; the full-capture branch is
  printed but not gated. **10GB has never been measured** — it remains, as the
  overview says, the first thing to validate.
- `/rewind` restores untracked + gitignored changes byte-identically after a
  `bash` side effect. **Met**, except mtimes, which are not restored.
- Agent can self-rollback via tool call. **Met.**
- Store growth over 100 checkpoints stays sub-linear. **Met**, gated at
  128 KiB per checkpoint.
- Kill -9 during checkpoint or restore leaves both recoverable. **Met.**

## Open questions (not yet settled)

1. **What happens when the store outgrows the disk?** There is no GC, no cap
   and no pruning; `status` reports size and nothing acts on it. *Current
   lean: an enforced cap is implementable today and a GC is not.*
2. **Should the tool matcher be policy?** A fixed list of six tool names
   decides what gets checkpointed. A host that adds a mutating tool is
   silently uncovered. *No lean.*
3. **Is the 2s pre-tool bound right, and should it be configurable?** On a
   slow capture the agent proceeds uncheckpointed and nothing says so. *No
   lean.*
4. **Hardcoded constants that are really policy** — mutation batch size,
   maximum capture paths, extent spans, watch queue depth, prompt excerpt
   bytes. None are configurable and none are documented.
5. **Excluded paths are invisible to forks and Safe Mode sessions.** A secret
   kept out of the store is also absent from every fork, which is either the
   correct safety property or a broken build, depending on the secret. *No
   lean.*
6. **Does "byte-identical" stay as a criterion** given mtimes are not
   restored? *Current lean: reword it rather than implement mtime restore.*

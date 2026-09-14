# Launch 1 — Rewind

The snapshot store. Story: **"never fear letting the agent loose."**

## Features

1. **Auto-checkpoint every agent action** — snapshot the tree before/after each edit or command; zero-config, invisible until needed. The foundation everything else sits on.
2. **Rewind for the dev** — restore the tree to any checkpoint, including untracked files, mutated dependencies, generated files — the state git can't give back.
3. **Agent self-rollback** — rollback as a tool the agent uses: try an approach, fail tests, revert cleanly, try approach 2 with an unpoisoned tree.
4. **Blast-radius diff** — after a long agent run, one view of everything that changed since the session started — an audit trail for reviewing agent work before committing.

## User journey

1. Maya runs `acyclic init` in her repo and starts Claude Code as usual. Nothing looks different — checkpointing is already on.
2. She asks for a risky change: "migrate the auth layer off sessions to JWTs." The agent edits 30 files, regenerates the API client, and runs a database migration script.
3. Tests fail in a way that smells structural. Instead of asking the agent to "undo it" (more edits, more drift), she types `/rewind` — the tree is back exactly as it was, including the generated client and gitignored artifacts git would never have restored.
4. She re-prompts with a constraint. Mid-task, the agent's second approach hits a dead end — it rolls *itself* back two checkpoints and tries a third route, without poisoning the tree with half-applied edits.
5. The approach works. Before committing, she opens the blast-radius diff: every file the session touched, including what the migration script wrote. She reviews it like a PR, then commits.

## What has to be built

- **Local content-addressed snapshot store** — chunked, deduplicated object store (git-plumbing-like, but covering untracked and gitignored files) in `.acyclic/` or a global per-machine store. Format must be dVFS-compatible from day one so cloud sync is a v2 toggle, not a rewrite.
- **Merkle-DAG tree representation** — directories are nodes hashed over their children; files chunked with content-defined chunking for large-file dedup. A checkpoint is a new root hash with structural sharing of every unchanged subtree — O(changed files), not O(repo). Comparing two checkpoints is a hash-guided descent skipping identical subtrees. This same structure later makes forks O(1) and dVFS sync a Merkle-diff exchange.
- **Incremental tree capture** — filesystem watcher (FSEvents/inotify) maintaining an mtime+hash manifest keeping the Merkle tree current between checkpoints; a checkpoint is a delta write in milliseconds, never a full repo walk. Must hold up on 10GB trees.
- **Host-tool hook integration** — Claude Code PreToolUse/PostToolUse hooks (and Codex equivalent) triggering checkpoints around Edit/Write/Bash. This is the "deep integration" surface; no filesystem driver needed yet.
- **Restore engine** — atomic materialization of any checkpoint back onto the working tree; handles deletes, permission bits, partially-restored states safely.
- **Diff engine + surfaces** — tree-to-tree diff for the blast-radius view; a slash command for the dev and a tool (CLI or MCP) the agent calls for self-rollback.
- **Retention/GC policy** — checkpoint pruning and store size caps so the store never becomes something devs manage.
- **Compliance-driven (design-in now, cannot retrofit)** — chunk-level deletion (purge-through-history), snapshot exclusion rules for secret paths, encryption at rest. See [07-compliance.md](07-compliance.md).

## Differentiation vs. native host checkpoints

Claude Code's native rewind tracks the agent's own file edits per turn. Ours must lead with what it can't do:

- Captures **what Bash did**: package installs, migration scripts, code generators, arbitrary side effects.
- Covers untracked and gitignored files.
- Survives across sessions.
- Identical behavior in Codex, OpenCode, and any shell-capable agent.
- Checkpoints are fork-able (Launch 3) and searchable (Launch 5).

## Acceptance criteria

- Checkpoint latency < 100ms p95 on a 1GB tree with a warm manifest; < 1s p95 on 10GB.
- `/rewind` restores a tree containing untracked + gitignored changes byte-identically, including after a `bash` side effect (verified via migration-script scenario).
- Agent can self-rollback via tool call without dev intervention.
- Store growth over a 100-checkpoint session stays sub-linear in tree size (dedup verified).
- Kill -9 during checkpoint or restore leaves both store and working tree recoverable.

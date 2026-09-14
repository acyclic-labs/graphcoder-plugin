# Launch 5 — Monorepo

The index engine. Dependency-independent — pull forward if monorepo teams become the target buyer.

## Features

14. **Indexed search** — content-indexed millisecond grep/glob over huge repos; the agent's existing search tools transparently accelerated.
15. **Repo map / structure cache** — a maintained symbol-and-dependency index the agent queries instead of re-deriving the codebase's shape every session.
16. **Warm session start** — state and index persist, so a new session in a monorepo is instantly oriented; no cold "let me look around" phase.

## User journey

1. Dev, a platform engineer, works in a 14GB monorepo where the team quietly stopped using coding agents — every session began with minutes of slow greps and wrong turns.
2. With the plugin installed, `acyclic init` builds the index once overnight; the watcher keeps it current from then on.
3. He starts a session. The agent opens already oriented — the injected repo brief covers the workspace layout, the service his ticket touches, and its dependency fan-in. No exploration phase.
4. The agent's searches over the full tree return in milliseconds instead of tens of seconds; a task that took 40 minutes of wall-clock last quarter takes 12.
5. Later, the agent searches inside last week's checkpoint to find when a symbol disappeared — the Merkle-aware index answers from that tree's state, not today's.
6. He posts the before/after in the team channel; the team turns agents back on for the monorepo.

## What has to be built

- **Incremental content index** — trigram/full-text index over the tree, updated by the same watcher that feeds the Merkle manifest; correct under rapid agent writes and cheap to keep warm.
- **Symbol and dependency indexer** — tree-sitter/LSP-based extraction of definitions, references, and import graphs into a queryable repo map, refreshed incrementally per changed file.
- **Tool interception** — routing the agent's existing Grep/Glob/read patterns through the index (hook-level rewrite or a shimmed search binary) so acceleration is transparent — the agent's behavior doesn't change, its latency does.
- **Index-per-checkpoint semantics** — searches against a fork or an old checkpoint must answer from that tree's state, not the live one; the index must be Merkle-aware (index chunks keyed by subtree hash, shared across checkpoints).
- **Startup context injection** — a compact, always-current repo brief (structure, entry points, conventions) injected at session start from the persistent index, replacing the cold exploration phase.

## Acceptance criteria

- Full-tree search on a 10GB repo returns in < 50ms p95 with a warm index; index update lag behind a write < 1s.
- Search against a historical checkpoint returns that checkpoint's results (verified with a deleted-symbol scenario).
- Repo brief stays under a fixed token budget and regenerates incrementally, never by full rescan.
- Fallback is graceful: if the index is cold or the host disallows interception, the agent's normal tools still work (just slower) and `acyclic search` is available explicitly.

## Open design risks

- Transparent tool interception is an unverified host-API assumption (rewrite limits differ across Claude Code / Codex / OpenCode). The fallback — teach the agent to prefer `acyclic search` via the skill/AGENTS.md — is acceptable but no longer "transparent." Validate early.

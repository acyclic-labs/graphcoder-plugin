# Launch 5 — Monorepo

The index engine. Dependency-independent — pull forward if monorepo teams
become the target buyer.

Status: **not started.** Nothing in this doc is built: there is no trigram
index, no tree-sitter extraction, and no `acyclic search` verb. Two notes
before reading it as a clean slate, both of which change the plan.

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
- **Startup context injection** — *half of this already ships.* The
  session-start hook slot, the 1KB budget and the renderer all exist as
  `acyclic brief`, which injects the *previous session's* end state. What does
  not exist is repo-structure content to put in it. This is an extension of a
  shipped mechanism, not a new one — and on MCP-only hosts there is no hook to
  inject into at all, so it would have to be a tool the model chooses to call.

**A name collision to resolve first:** "the index engine" is what this doc
calls Launch 5, but `index` already names a shipped component — the checkpoint
metadata index (SQLite, WAL, owned by the daemon). The README already
describes the shipped CLI as "watcher, Merkle-DAG snapshot store, index",
which reads as though Launch 5 exists.

## Acceptance criteria

- Full-tree search on a 10GB repo returns in < 50ms p95 with a warm index; index update lag behind a write < 1s.
- Search against a historical checkpoint returns that checkpoint's results (verified with a deleted-symbol scenario).
- Repo brief stays under a fixed token budget and regenerates incrementally, never by full rescan.
- Fallback is graceful: if the index is cold or the host disallows
  interception, the agent's normal tools still work (just slower) and
  `acyclic search` is available explicitly. *No `search` subcommand exists.*

## Open design risks

- Transparent tool interception is an unverified host-API assumption. Partly
  answerable already from what shipped: the hook layer is **not** a rewrite
  layer — it always exits 0 and never mutates tool input — and the only place
  hook stdout reaches the model is `SessionStart`/`UserPromptSubmit`. So
  interception as designed is not reachable through the adapter that exists.
  The fallback — teach the agent to prefer `acyclic search` — is acceptable
  but no longer "transparent."

## Open questions (not yet settled)

1. **Baseline capture cost is the real monorepo blocker and was never listed.**
   `init` runs at roughly 230 s/GiB, so the journey's 14GB repo is a ~50-minute
   first run before any index work begins. Every claim in this launch sits
   behind that number. *Current lean: this is the first thing to fix, ahead of
   any indexing.*
2. **The 10GB target has no harness.** There is no search-shaped or 10GB-shaped
   acceptance test, and the store has never been measured at that size — it is
   still the overview's "first thing to validate". *No lean.*
3. **`exclude` blinds any future index.** Excluded paths never enter a
   checkpoint, so a Merkle-aware index can never answer for them, and the
   answer it gives will be silently incomplete rather than refused. *No lean.*
4. **Forks and Safe Mode sessions do not see excluded paths either**, which
   bears directly on "searches against a fork must answer from that tree's
   state". *No lean.* The converse is now a known gap: paths *created* inside
   a fork are captured regardless of `exclude` — see
   [fork-writes-bypass-exclude.md](fork-writes-bypass-exclude.md).
5. **Does the name change?** Calling this "the index engine" collides with the
   shipped metadata index. *Current lean: rename this launch, not the shipped
   component.*
6. **Is Launch 5 really dependency-independent?** The overview says so, but
   index-per-checkpoint semantics depend on the Merkle store and on fork
   materialization from Launches 1 and 3. *Current lean: no — the claim
   should be dropped.*

# Overview

> Source of truth for product framing: the [Acyclic plugins docs page](https://acyclic.dev/docs/plugins). This directory is the engineering-facing expansion of that spec.

## Thesis

**A local product with plugin distribution.** The product is an agent-native state engine that runs on the dev's machine — snapshots, forks, and indexing over the working tree. The plugins are thin adapters that deliver it through Claude Code, Codex, OpenCode, and any agent that can run a shell command. The engine is the moat; the plugins are the channel.

**Local-first cockpit, agent-summoned muscle.** Typing `claude` (or `codex`) starts an ordinary local session — the dev's terminal, their repo, their workflow, unchanged in minute one. The plugin upgrades the substrate the agent works against, not where the agent lives.

**V1 is entirely local**: no sandboxes, no managed sessions, no cloud sync. Compute offload (sandboxes) and durable sessions (managed agents) arrive in later versions, reached incrementally from the same local session.

## Position in the Acyclic roadmap

The plugin's parents in the dependency graph: **Sandboxes**, **Managed Agents**, and **VCS/dVFS**. V1 draws only on the VCS/dVFS lineage (as a local engine); the other two parents connect in v2 via the dVFS bridge — the store format is dVFS-compatible from day one so cloud sync is a toggle, not a rewrite.

Platform reality at time of writing: sandboxes and managed sessions work; dVFS does not yet. Local v1 is chosen for product/trust reasons, not readiness — the engine here is the freshest-built piece of the stack.

## Launch sequence

Each launch is one engine increment plus one coherent story, ordered by dependency:

| Launch | Name | Engine increment | Story | Plan |
|---|---|---|---|---|
| 1 | Rewind | Merkle snapshot store + host hooks | Never fear letting the agent loose | [01-rewind.md](01-rewind.md) |
| 2 | Timeline | Turn-linked metadata index | The repo at any point in the conversation | [02-timeline.md](02-timeline.md) |
| 3 | Forks | Copy-on-write materialization | N parallel attempts, pick the winner | [03-forks.md](03-forks.md) |
| 5 | Monorepo | Merkle-aware content + symbol index | The repo that finally works with agents | [05-monorepo.md](05-monorepo.md) |

Cross-cutting, not a launch: **[Speculation](08-speculation.md)** — the daemon computes what the agent is about to ask for (the session brief, a turn summary) in the time when nobody is waiting. Off by default.

Sequencing rationale:

- Launches 1–2 ship on the cheap engine (hooks + snapshot store) without committing to how forks work. The hard CoW decision only becomes due at Launch 3.
- Launch 5 is dependency-independent (separate index engine) — pull it forward if monorepo teams become the target buyer.

## The load-bearing decision

The **Merkle DAG** is what makes checkpoints O(changed files) in Launch 1, forks free in Launch 3, per-checkpoint search coherent in Launch 5, and dVFS sync a hash-exchange in v2. The first thing to validate: store performance on a real 10GB tree.

Second load-bearing constraint, from compliance: **purge-through-history and snapshot exclusions must be designed into the store from Launch 1** — content-addressed stores make retroactive deletion hard to retrofit (see [07-compliance.md](07-compliance.md)).

## Strategic risks (from the design review)

1. **Launch 1 collides with native host features.** Claude Code ships its own checkpoint/rewind. Differentiation must be the headline, not fine print: we capture what Bash did (installs, migrations, generated files), untracked/gitignored state, cross-session persistence, cross-host consistency — and checkpoints become forks. Expect hosts to keep commoditizing the basic rewind; the moat is the Merkle/CoW engine and Launches 3/5.
2. **Unverified host-API assumptions** — validate before public promises: (a) transparent tool interception for indexed search (hook rewrite limits differ per host); (b) checkpoint alignment in hosts without lifecycle hooks.
3. **Naming**: repo is `graphcoder-plugin`, CLI is `acyclic`, and Graphcoder is a different product on the roadmap. Resolve before launch.

## Settled since this was written

1. **Fork engine mechanism** — mounts shipped. Forks require the native provider and are routes inside one kernel mount rather than N mounts. See `implementation-forks.md`.
2. **Checkpoint alignment in hosts without lifecycle hooks** — resolved by the MCP adapter plus the `auto_checkpoint_idle_ms` timer. MCP is a second adapter shape this doc's thesis line does not yet mention.

## Open questions (not yet settled)

1. **Git relationship**: invisible layer (own store, never touches git state) vs. git-integrated (hidden refs) vs. designed-to-replace-git. Current lean: invisible layer for v1. Note two read-side couplings that already exist and complicate "invisible": `.git` is captured but filtered out of blast-radius diffs, and merge shells out to `git check-ignore`.
2. **Cloud tether**: zero cloud vs. account+telemetry vs. optional snapshot backup. The code currently has *no* network surface at all, so "zero cloud" is the de facto state rather than a choice that was made.
3. **Naming**: repo is `graphcoder-plugin`, CLI is `acyclic`, and Graphcoder is a different product on the roadmap. Still unresolved.
4. **Repo visibility** — this repo is private while its `acyclic-fs` dependency is public, which blocks the curl installer, the attestations, and the open-source claim. Carried in `06-installation.md` and `07-compliance.md`; it belongs at overview level because it gates positioning, not just packaging.
5. **The upstream retention dependency.** GC, purge and enforced retention all wait on an `acyclic-fs` retention-release fact that does not exist. This is the single largest gap between the compliance story and the code, and it is not ours to close.
7. **What remains unvalidated at scale.** Store performance on a real 10GB tree is still the first thing to validate, as it was when this doc was written. The latency gate runs 20k files / 256 MB, and `init` baseline capture runs ~230 s/GiB.

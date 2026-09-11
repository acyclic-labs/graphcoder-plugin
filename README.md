# graphcoder-plugin

**A local product with plugin distribution.** The product is an agent-native state engine that runs on your machine — snapshots, forks, and indexing over your working tree. The plugins are thin adapters that deliver it through Claude Code, Codex, OpenCode, and any agent that can run a shell command. The engine is the moat; the plugins are the channel.

> Status: Launches 1–4 built (Rewind, Timeline, Forks, Safe Mode), acceptance suites green on macOS and Linux, packaged for npm as `@acyclic-labs/plugin`. Remaining before public release: Phase 4 hardening (retention/GC, snapshot exclusions and purge, signed artifacts, install.sh). Launch 5 (Monorepo) is spec. The spec lives on the [Acyclic plugins docs page](https://acyclic.dev/docs/plugins).

## Thesis

Local-first cockpit, agent-summoned muscle. Typing `claude` (or `codex`) starts an ordinary local session — the dev's terminal, their repo, their workflow, unchanged in minute one. The plugin upgrades the substrate the agent works against, not where the agent lives.

V1 is entirely local: no sandboxes, no managed sessions, no cloud sync. It ships the state layer — snapshots, forks, and indexing — drawn from dVFS and the agent-native VCS. Compute offload and durable sessions arrive in later versions, reached incrementally from the same local session.

## Architecture

One engine, thin adapters:

- **`acyclic` CLI + daemon** — watcher, Merkle-DAG snapshot store, index. Host-agnostic.
- **Per-host adapters** — Claude Code (built: hooks, `/rewind` `/timeline` `/fork`, two skills; `acyclic install claude-code`), anything shell-capable (built: `acyclic install agents-md`), Codex and OpenCode (planned).

## Launch plan

| Launch | Name | Engine increment | Story | Status |
|---|---|---|---|---|
| 1 | Rewind | Merkle snapshot store + host hooks | Never fear letting the agent loose | built (`tests/acceptance/journey.sh`, `crash.sh`, `soak.sh`, `latency.sh`, `claude-e2e.sh`) |
| 2 | Timeline | Turn-linked metadata index | The repo at any point in the conversation | built (`timeline.sh`) |
| 3 | Forks | Copy-on-write materialization | N parallel attempts, pick the winner | built: mounted forks, promote with three-way merge (`forks.sh`, `merge.sh`, `claude-merge-e2e.sh`) |
| 4 | Safe Mode | Session redirection + interposition | Agents on the codebase, not agents' mistakes in it | built, needs the native mount layer (`safe-mode.sh`) |
| 5 | Monorepo | Merkle-aware content + symbol index | The repo that finally works with agents | not started |

Run everything with `tests/acceptance/run-all.sh`; the live Claude Code scenarios are gated by `ACYCLIC_E2E=1`.

Full feature lists, user journeys, and technical requirements per launch: [docs/plugins](https://acyclic.dev/docs/plugins).

## Compliance posture

Local-first (no code leaves the machine in v1), verifiable open source (signed reproducible releases, SBOM, SLSA provenance), and first-class controls on the snapshot store (encryption at rest, snapshot exclusions, purge-through-history, team-enforced retention). See the compliance section of the docs page.

Note for Launch 1 design: purge-through-history and snapshot exclusions must be designed into the Merkle store from the start — content-addressed stores make retroactive deletion hard to retrofit.

## License

Apache-2.0 (see [LICENSE](LICENSE)). Contributions require DCO sign-off.

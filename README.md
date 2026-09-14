# graphcoder-plugin

**A local product with plugin distribution.** The product is an agent-native state engine that runs on your machine — snapshots, forks, and indexing over your working tree. The plugins are thin adapters that deliver it through Claude Code, Codex, OpenCode, and any agent that can run a shell command. The engine is the moat; the plugins are the channel.

> Status: design phase. The spec lives on the [Acyclic plugins docs page](https://acyclic.dev/docs/plugins); this repo is where it becomes code.

## Thesis

Local-first cockpit, agent-summoned muscle. Typing `claude` (or `codex`) starts an ordinary local session — the dev's terminal, their repo, their workflow, unchanged in minute one. The plugin upgrades the substrate the agent works against, not where the agent lives.

V1 is entirely local: no sandboxes, no managed sessions, no cloud sync. It ships the state layer — snapshots, forks, and indexing — drawn from dVFS and the agent-native VCS. Compute offload and durable sessions arrive in later versions, reached incrementally from the same local session.

## Architecture

One engine, thin adapters:

- **`acyclic` CLI + daemon** — watcher, Merkle-DAG snapshot store, index. Host-agnostic.
- **Per-host adapters** — Claude Code (hooks + slash commands + skill), Codex (AGENTS.md + CLI/MCP), OpenCode (plugin), anything else (`acyclic install --agents-md`).

## Launch plan

| Launch | Name | Engine increment | Story |
|---|---|---|---|
| 1 | Rewind | Merkle snapshot store + host hooks | Never fear letting the agent loose |
| 2 | Timeline | Turn-linked metadata index | The repo at any point in the conversation |
| 3 | Forks | Copy-on-write materialization | N parallel attempts, pick the winner |
| 4 | Safe Mode | Session redirection + interposition | Agents on the codebase, not agents' mistakes in it |
| 5 | Monorepo | Merkle-aware content + symbol index | The repo that finally works with agents |

Full feature lists, user journeys, and technical requirements per launch: [docs/plugins](https://acyclic.dev/docs/plugins).

## Compliance posture

Local-first (no code leaves the machine in v1), verifiable open source (signed reproducible releases, SBOM, SLSA provenance), and first-class controls on the snapshot store (encryption at rest, snapshot exclusions, purge-through-history, team-enforced retention). See the compliance section of the docs page.

Note for Launch 1 design: purge-through-history and snapshot exclusions must be designed into the Merkle store from the start — content-addressed stores make retroactive deletion hard to retrofit.

## License

Apache-2.0 (see [LICENSE](LICENSE)). Contributions require DCO sign-off.

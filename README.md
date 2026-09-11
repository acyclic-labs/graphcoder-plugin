# graphcoder-plugin

Checkpoint every agent action, rewind exactly, see the blast radius. The store captures what git can't give back: untracked files, gitignored artifacts, and what a `bash` step wrote. History survives across sessions and is linked to the conversation turn that caused it.

**A local product with plugin distribution.** The product is an agent-native state engine that runs on your machine — snapshots, forks, and indexing over your working tree. The plugins are thin adapters that deliver it through Claude Code, Codex, OpenCode, and any agent that can run a shell command. The engine is the moat; the plugins are the channel.

> Status: Launches 1–4 built (Rewind, Timeline, Forks, Safe Mode), acceptance suites green on macOS and Linux, published to npm as `@acyclic-labs/plugin`. Launch 1's release gate is met: snapshot exclusions, a store-growth proof, license scanning, an attested SBOM per binary, `scripts/install.sh`, and a clean-machine install test. What v1 deliberately does not do is prune or purge history; see [Retention and purge](#retention-and-purge). Launch 5 (Monorepo) is spec. The spec lives on the [Acyclic plugins docs page](https://acyclic.dev/docs/plugins).

## Install

```sh
npm i -g @acyclic-labs/plugin                                                       # prebuilt binary, macOS + Linux
curl -fsSL https://raw.githubusercontent.com/acyclic-labs/graphcoder-plugin/main/scripts/install.sh | sh   # or: verified download into ~/.local/bin
cd your-repo
acyclic init                       # starts the daemon, builds the first snapshot
acyclic install claude-code        # hooks, /rewind /timeline /fork, two skills; checked in
```

Any shell-capable agent can use the CLI directly; `acyclic install agents-md` teaches it the verbs. Releases are built natively per target, carry SLSA build-provenance and SBOM attestations, and ship a `SHA256SUMS` the installer verifies. Cutting one is described in `packaging/npm/RELEASING.md`.

## The public name

`product.toml` at the repo root holds the public name once. The CLI command, `.<name>/config.toml`, the state and config directories, hook commands, skill names, message prefixes, the `<NAME>_TRACE` and `<NAME>_HOOK` variables, release asset names, and the npm bin all derive from it at build or packaging time (`crates/acyclic-engine/build.rs`, `scripts/product.sh`, the workflows). Crate names stay `acyclic*` because they are internal. `scripts/install.sh` is fetched standalone and mirrors the name; `scripts/check-product-name.sh` fails CI if it drifts or if any user-facing Rust string spells the name out. Renaming is: change `product.toml`, update the two mirror lines in `install.sh`, rebuild.

## Configuration

`.acyclic/config.toml` is checked in, so the policy ships with the repo. Every key has a safe default; zero config is supported. Machine-level defaults live in `~/.config/acyclic/config.toml`.

| Key | Default | What it does |
|---|---|---|
| `exclude` | `[]` | Repo-relative paths (a file, or a directory and everything under it) that never enter a checkpoint: secrets, bulky generated state. A rewind leaves the live copies untouched; `acyclic restore` refuses them. |
| `trash_ttl_days` | `7` | How long a rewound-away tree stays in the store's trash. |
| `commit_every` / `commit_idle_ms` | `25` / `60000` | How often per-tool-call checkpoints are published to the durable store. |
| `quiesce_ms` / `quiesce_cap_ms` | `50` / `500` | Watcher quiet window before a capture. |
| `dry_run` / `guarded_paths` | `false` / `[]` | Safe Mode (Launch 4). |
| `[decompose]` / `[merge]` | | Fork decomposition policy and merge limits (Launch 3). |
| `store_dir` | `~/.local/share/acyclic/stores` | Where stores live. Never inside the repo. |

Adding a path to `exclude` takes effect at the next daemon start; the baseline it builds is scrubbed, and every later checkpoint skips the path. Generations captured before the rule still hold it (see below).

## Retention and purge

`acyclic status` reports the store size; trash is pruned by TTL. The store itself is never garbage-collected in v1, on purpose. At the pinned `acyclic-fs` revision a generation stays reachable only while it is a workspace head or carries a retention fact (checkpoint label, pin, fork base), retention facts cannot be released, and closure proofs do not follow generation parents. So the fs collector would either destroy every checkpoint but the head or, if every checkpoint were pinned first, never free anything again. Purge-through-history has the same dependency: content cannot be physically removed from a retained generation. Both land when the fs grows a retention-release fact; until then, keep secrets out of the store with `exclude`, which is the compliance control that ships. Details and the upstream ask are in `plans/implementation-rewind.md`, Phase 4.

Known caveats: mtimes are not restored on rewind, a rewind warrants an editor reload, forks and Safe Mode sessions do not see excluded paths, and baseline capture runs at roughly 230 s/GiB on first `init`.

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

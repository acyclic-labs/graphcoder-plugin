# graphcoder-plugin

Checkpoint every agent action, rewind exactly, see the blast radius. The store captures what git can't give back: untracked files, gitignored artifacts, and what a `bash` step wrote. History survives across sessions and is linked to the conversation turn that caused it.

**A local product with plugin distribution.** The product is an agent-native state engine that runs on your machine — snapshots, forks, and indexing over your working tree. The plugins are thin adapters that deliver it through Claude Code, Codex, OpenCode, any agent that can run a shell command, and Claude Desktop over MCP. The engine is the moat; the plugins are the channel.

> Status: Launches 1–4 built (Rewind, Timeline, Forks, Safe Mode), acceptance suites green on macOS and Linux, published to npm as `@acyclic-labs/plugin`. Launch 1's release gate is met: snapshot exclusions, a store-growth proof, license scanning, an attested SBOM per binary, `scripts/install.sh`, and a clean-machine install test. What v1 deliberately does not do is prune or purge history; see [Retention and purge](#retention-and-purge). Launch 5 (Monorepo) is spec. The spec lives on the [Acyclic plugins docs page](https://acyclic.dev/docs/plugins).

## Install

Get the binary, start the daemon in your repo, then wire in each coding tool you use:

```sh
npm i -g @acyclic-labs/plugin                                                       # prebuilt binary, macOS + Linux
curl -fsSL https://raw.githubusercontent.com/acyclic-labs/graphcoder-plugin/main/scripts/install.sh | sh   # or: verified download into ~/.local/bin
cd your-repo
acyclic init                       # starts the daemon, builds the first snapshot
acyclic install <host>             # one of the hosts below; repeat per tool you use
```

Releases are built natively per target, carry SLSA build-provenance and SBOM attestations, and ship a `SHA256SUMS` the installer verifies. Cutting one is described in `packaging/npm/RELEASING.md`.

### Per host

Two adapter shapes exist. **Hook-based** hosts expose a lifecycle-hook API, so a checkpoint is taken automatically around every edit and command; the adapter is checked-in config the whole team inherits. **MCP-based** hosts have no such API; the adapter registers `acyclic mcp`, an MCP server that exposes `checkpoint`/`timeline`/`rewind`/`diff`/`restore`/`turns`/`brief` as tools the model calls explicitly, and the daemon's idle timer (`auto_checkpoint_idle_ms`) catches edits nothing asked to checkpoint.

| Host | Surface | Shape | Command | What it writes | Verified |
|---|---|---|---|---|---|
| Claude Code | CLI | hooks | `acyclic install claude-code` | `.claude/settings.json` hooks, `/rewind` `/timeline` `/fork` commands, two skills — checked in | live session: `tests/acceptance/claude-e2e.sh` |
| Codex | CLI | hooks | `acyclic install codex` | `.codex/hooks.json` + AGENTS.md cheatsheet — checked in | live session: `codex-e2e.sh` |
| Cursor | desktop app + CLI | hooks + MCP | `acyclic install cursor` | `.cursor/hooks.json`, `.cursor/rules/acyclic.mdc`, `.cursor/mcp.json` — checked in | hooks, live session: `cursor-e2e.sh`. MCP: server side only (`mcp-e2e.sh`); Cursor reading `.cursor/mcp.json` not yet exercised |
| Any shell-capable agent | CLI | cheatsheet | `acyclic install agents-md` | AGENTS.md block — checked in | n/a: no host to drive. Checkpoints come from the idle timer, not hooks |
| Claude Desktop | desktop app | MCP | `acyclic install claude-desktop` | `mcpServers.acyclic` in your global `claude_desktop_config.json` — **per machine and per repo, not checked in**; restart Desktop afterwards | server side: `mcp-e2e.sh` on every CI run; config merge: unit-tested and run against a real config. A tool call from inside the app: not yet |
| VS Code (Copilot agent mode) | IDE | MCP | `acyclic install vscode` | `.vscode/mcp.json` — checked in | server side: `mcp-e2e.sh`; config shape from VS Code's docs, unit-tested. VS Code reading it: not yet |

"Verified" means what CI or a person has actually run, not what should work. The `*-e2e.sh` live sessions need the host CLI and credentials and run behind `ACYCLIC_E2E=1`; `mcp-e2e.sh` drives `acyclic mcp` with a scripted client and needs only the binary, so it runs on every CI pass. The MCP hosts share one honest gap: nothing yet drives the real app to click a tool. The install-side config merges are unit-tested in `crates/acyclic/src/install.rs`.

Not yet covered: Codex's own MCP path (its MCP config is TOML, and its hooks may already reach the ChatGPT desktop app and IDE extension, which read the same config), Kimi Code CLI, Windsurf, Zed, JetBrains AI assistants, Gemini CLI, Amazon Q Developer, OpenCode. The `TODO(more hosts)` block above `adapters()` in `install.rs` is the checklist for adding one: find the host's hook or MCP config from its own docs, reuse `merge_mcp_server_json` when the shape fits, add a unit test that proves other entries survive, then verify against the real app.

## The public name

`product.toml` at the repo root holds the public name once. The CLI command, `.<name>/config.toml`, the state and config directories, hook commands, skill names, message prefixes, the `<NAME>_TRACE` and `<NAME>_HOOK` variables, release asset names, and the npm bin all derive from it at build or packaging time (`crates/acyclic-engine/build.rs`, `scripts/product.sh`, the workflows). Crate names stay `acyclic*` because they are internal. `scripts/install.sh` is fetched standalone and mirrors the name; `scripts/check-product-name.sh` fails CI if it drifts or if any user-facing Rust string spells the name out. Renaming is: change `product.toml`, update the two mirror lines in `install.sh`, rebuild.

## Configuration

`.acyclic/config.toml` is checked in, so the policy ships with the repo. Every key has a safe default; zero config is supported. Machine-level defaults live in `~/.config/acyclic/config.toml`.

| Key | Default | What it does |
|---|---|---|
| `exclude` | `[]` | Repo-relative paths (a file, or a directory and everything under it) that never enter a checkpoint: secrets, bulky generated state. A rewind leaves the live copies untouched; `acyclic restore` refuses them. |
| `trash_ttl_days` | `7` | How long a rewound-away tree stays in the store's trash. |
| `commit_every` / `commit_idle_ms` | `25` / `60000` | How often per-tool-call checkpoints are published to the durable store. |
| `auto_checkpoint_idle_ms` | `5000` | Idle-timer safety net: checkpoints changes on its own once the watcher has been quiet this long, for hosts with no lifecycle-hook API (Claude Desktop). `0` disables it. Cheap no-op for hooked hosts, which already drain the watcher themselves. |
| `quiesce_ms` / `quiesce_cap_ms` | `50` / `500` | Watcher quiet window before a capture. |
| `dry_run` / `guarded_paths` | `false` / `[]` | Safe Mode (Launch 4). |
| `[decompose]` / `[merge]` | | Fork decomposition policy and merge limits (Launch 3). |
| `store_dir` | `~/.local/share/acyclic/stores` | Where stores live. Never inside the repo. |

Adding a path to `exclude` takes effect at the next daemon start; the baseline it builds is scrubbed, and every later checkpoint skips the path. Generations captured before the rule still hold it (see below).

## Retention and purge

`acyclic status` reports the store size; trash is pruned by TTL. The store itself is never garbage-collected in v1, on purpose. At the pinned `acyclic-fs` revision a generation stays reachable only while it is a workspace head or carries a retention fact (checkpoint label, pin, fork base), retention facts cannot be released, and closure proofs do not follow generation parents. So the fs collector would either destroy every checkpoint but the head or, if every checkpoint were pinned first, never free anything again. Purge-through-history has the same dependency: content cannot be physically removed from a retained generation. Both land when the fs grows a retention-release fact; until then, keep secrets out of the store with `exclude`, which is the compliance control that ships. Details and the upstream ask are in `docs/design/implementation-rewind.md`, Phase 4.

Known caveats: mtimes are not restored on rewind, a rewind warrants an editor reload, forks and Safe Mode sessions do not see excluded paths, and baseline capture runs at roughly 230 s/GiB on first `init`.

## Thesis

Local-first cockpit, agent-summoned muscle. Typing `claude` (or `codex`) starts an ordinary local session — the dev's terminal, their repo, their workflow, unchanged in minute one. The plugin upgrades the substrate the agent works against, not where the agent lives.

V1 is entirely local: no sandboxes, no managed sessions, no cloud sync. It ships the state layer — snapshots, forks, and indexing — drawn from dVFS and the agent-native VCS. Compute offload and durable sessions arrive in later versions, reached incrementally from the same local session.

## Architecture

One engine, thin adapters:

- **`acyclic` CLI + daemon** — watcher, Merkle-DAG snapshot store, index. Host-agnostic.
- **Per-host adapters** — hook-based for CLIs with a lifecycle-hook API (Claude Code, Codex, Cursor), MCP-based for desktop apps and IDEs without one (Claude Desktop, VS Code; Cursor gets both). Every adapter is a `HostAdapter` in `crates/acyclic/src/install.rs`; the MCP server itself is `crates/acyclic/src/mcp.rs`, a thin translation of each tool call into the same `acyclic-proto::Op` the hooks send. The table under [Install](#per-host) says what each one writes and how far it has been verified; `docs/design/06-installation.md` has the design and the ship decision for the MCP path.

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

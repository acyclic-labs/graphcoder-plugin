# graphcoder-plugin

Checkpoint every agent action, rewind exactly, see the blast radius. The store captures what git can't give back: untracked files, gitignored artifacts, and what a `bash` step wrote. History survives across sessions and is linked to the conversation turn that caused it.

**A local product with plugin distribution.** The product is an agent-native state engine that runs on your machine — snapshots, forks, and indexing over your working tree. The plugins are thin adapters that deliver it through Claude Code, Codex, OpenCode, any agent that can run a shell command, and Claude Desktop over MCP. The engine is the moat; the plugins are the channel.

> Status: Launches 1–4 built (Rewind, Timeline, Forks, Safe Mode), acceptance suites green on macOS and Linux, published to npm as `@acyclic-labs/plugin`. Launch 1's release gate is met: snapshot exclusions, a store-growth proof, license scanning, an attested SBOM per binary, `scripts/install.sh`, and a clean-machine install test. What v1 deliberately does not do is prune or purge history; see [Retention and purge](#retention-and-purge). Launch 5 (Monorepo) is spec. The spec lives on the [Acyclic plugins docs page](https://acyclic.dev/docs/plugins).

## Install

**Step 1 — get the binary.** Pick one of these; they are alternatives, not a sequence.

```sh
npm i -g @acyclic-labs/plugin
```

```sh
curl -fsSL https://raw.githubusercontent.com/acyclic-labs/graphcoder-plugin/main/scripts/install.sh | sh
```

npm ships prebuilt binaries for macOS, Linux, and Windows x64, and is the path that works today. The installer script, which covers macOS and Linux only, downloads the binary for your machine from a GitHub release, checks it against that release's `SHA256SUMS`, and drops it in `~/.local/bin` — no sudo, no package manager. **No release is cut yet, so the script currently exits with a 404 and points you back at npm**; it goes live with the first tag. Set `ACYCLIC_VERSION` to pin a release and `ACYCLIC_INSTALL_DIR` to install elsewhere.

**Step 2 — wire it into your repo.**

```sh
cd your-repo
acyclic init            # starts the daemon, builds the first snapshot
acyclic install <host>  # one of the hosts below; repeat per tool you use
```

Releases are built natively per target, carry SLSA build-provenance and SBOM attestations, and ship a `SHA256SUMS` the installer verifies. Cutting one is described in `packaging/npm/RELEASING.md`.

### Per host

Two adapter shapes exist. **Hook-based** hosts expose a lifecycle-hook API, so a checkpoint is taken automatically around every edit and command; the adapter is checked-in config the whole team inherits. **MCP-based** hosts have no such API; the adapter registers `acyclic mcp`, an MCP server that exposes `checkpoint`/`timeline`/`rewind`/`diff`/`restore`/`turns`/`brief` as tools the model calls explicitly, and the daemon's idle timer (`auto_checkpoint_idle_ms`) catches edits nothing asked to checkpoint.

| Host | Surface | Shape | Command | What it writes | Verified |
|---|---|---|---|---|---|
| Claude Code | CLI | hooks | `acyclic install claude-code` | `.claude/settings.json` hooks, `/rewind` `/timeline` `/fork` commands, two skills — checked in | live session: `tests/acceptance/claude-e2e.sh` (2.1.270, `claude-opus-5`). Also drives `acyclic mcp` as an MCP client: `mcp-clients-e2e.sh` |
| Codex | CLI | hooks | `acyclic install codex` | `.codex/hooks.json` + AGENTS.md cheatsheet — checked in; trust the hooks once via `/hooks` | live session: `codex-e2e.sh` (0.154.0). MCP client path via `config.toml` overrides: `mcp-clients-e2e.sh` |
| Cursor | desktop app + CLI | hooks + MCP | `acyclic install cursor` | `.cursor/hooks.json`, `.cursor/rules/acyclic.mdc`, `.cursor/mcp.json` — checked in and portable: bare `acyclic` from `PATH`, and the server finds the repo from its working directory | hooks, live session: `cursor-e2e.sh`. MCP: `cursor-agent` lists the tools and calls them, `mcp-clients-e2e.sh` (after `cursor-agent mcp enable acyclic`) |
| Any shell-capable agent | CLI | cheatsheet | `acyclic install agents-md` | AGENTS.md block — checked in | n/a: no host to drive. Checkpoints come from the idle timer, not hooks |
| Claude Desktop | desktop app | MCP | `acyclic install claude-desktop` | `mcpServers.acyclic-<repo name>` in your global `claude_desktop_config.json`, one entry per repo — **per machine, not checked in**; restart Desktop afterwards | the real app launches the server and completes `initialize` + `tools/list` (checked in its MCP log); server side: `mcp-e2e.sh` on every CI run. A tool call from inside a chat: manual only, see the guide |
| VS Code (Copilot agent mode) | IDE | MCP | `acyclic install vscode` | `.vscode/mcp.json` — checked in and portable: bare `acyclic` from `PATH`, and the server finds the repo from its working directory | server side: `mcp-e2e.sh`; config shape from VS Code's docs, unit-tested. VS Code reading it: not yet |
| OpenCode | CLI | MCP | `acyclic install opencode` | `opencode.json` (`mcp.acyclic`, `type: "local"`, command-as-array) + AGENTS.md cheatsheet — checked in and portable: bare `acyclic` from `PATH` | `opencode mcp list` reports `✓ acyclic connected` against the written config (2026-09-16); server side: `mcp-e2e.sh`. No in-chat tool call yet |
| GitHub Copilot CLI | CLI | MCP | `acyclic install copilot` | `mcpServers.acyclic-<repo name>` in your global `~/.copilot/mcp-config.json` (`$COPILOT_HOME` moves it), one entry per repo — **per machine, not checked in** | server side: `mcp-e2e.sh`; config shape from GitHub's docs, unit-tested. A live `copilot` session: `mcp-clients-e2e.sh` runs it when the CLI is on `PATH` — not yet exercised here (CLI not installed) |
| Copilot coding agent (cloud) | GitHub-hosted | MCP | `acyclic install copilot-agent` | `.github/workflows/copilot-setup-steps.yml` — checked in; the MCP entry itself is **printed to paste** into the repo's Copilot settings, since GitHub stores it there rather than in a file | not verified. The agent runs in a GitHub-hosted sandbox, so its checkpoints are that sandbox's timeline, not your local one — see the caveat below |

The Copilot coding agent is the one host that is not a local integration: it runs on GitHub's infrastructure against its own checkout, so `acyclic` there records that sandbox's work, and nothing it checkpoints reaches your machine unless it lands on a branch. `install copilot-agent` prepares that sandbox and prints the config to paste; it does not pretend to give you local parity.

"Verified" means what CI or a person has actually run, not what should work. The live sessions pin their model (`ACYCLIC_E2E_CLAUDE_MODEL`, `ACYCLIC_E2E_CURSOR_MODEL`, `ACYCLIC_E2E_CODEX_MODEL` in `common.sh`) because they assert on model-driven behavior — that the agent calls the tool rather than shelling out, and performs the prompt's steps in order — so an unpinned default makes a vendor's model change look like a regression. The Claude gate is pinned to `claude-opus-5`: it is green there and red on `claude-sonnet-5`, which does not complete the three-step prompt. Codex is unpinned (its CLI exposes no way to enumerate valid ids offline). The `*-e2e.sh` live sessions need the host CLI and credentials and run behind `ACYCLIC_E2E=1`; `mcp-e2e.sh` drives `acyclic mcp` with a scripted client and needs only the binary, so it runs on every CI pass. The install-side config merges are unit-tested in `crates/acyclic/src/install.rs`. [`docs/manual-testing.md`](docs/manual-testing.md) is the step-by-step checklist for re-verifying every host by hand after a host upgrade, with the results of the last pass.

Not yet covered: an `install` writer for Codex's MCP config (TOML), Kimi Code CLI, Windsurf, Zed, JetBrains AI assistants, Gemini CLI, Amazon Q Developer. The `TODO(more hosts)` block above `run()` in `install.rs` is the checklist for adding one: find the host's hook or MCP config from its own docs, reuse `merge_mcp_server_json` when the shape fits, add a unit test that proves other entries survive, then verify against the real app.

## The public name

`product.toml` at the repo root holds the public name once. The CLI command, `.<name>/config.toml`, the state and config directories, hook commands, skill names, message prefixes, the `<NAME>_TRACE` and `<NAME>_HOOK` variables, release asset names, and the npm bin all derive from it at build or packaging time (`crates/acyclic-engine/build.rs`, `scripts/product.sh`, the workflows). Crate names stay `acyclic*` because they are internal. `scripts/install.sh` is fetched standalone and mirrors the name, repo, and npm package; `scripts/check-product-name.sh` fails CI if any of them drifts or if any user-facing Rust string spells the name out. Renaming is: change `product.toml`, update the three mirror lines in `install.sh`, rebuild.

## Configuration

`.acyclic/config.toml` is checked in, so the policy ships with the repo. Every key has a safe default; zero config is supported. Machine-level defaults live in `~/.config/acyclic/config.toml`, and the two layers merge key by key — a repo config overrides only the keys it names.

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

Speculation is configured separately, in `~/.config/acyclic/speculate.toml` — per developer, never checked in, because turning it on can spend that developer's money. See [Speculation](#speculation).

Adding a path to `exclude` takes effect at the next daemon start; the baseline it builds is scrubbed, and every later checkpoint skips the path. Generations captured before the rule still hold it (see below).

## Speculation

The daemon knows things a request does not: it sees a prompt before the agent acts, it knows when a turn closed, and it knows when the watcher went quiet. Those are moments when the machine is idle and the answer to a question nobody has asked yet is already determined. So it computes them then.

- **The session brief**, when a session ends. It is the most expensive thing the agent waits on — `SessionStart` blocks on it and prints it into the model's context — and it costs a pipeline diff per abandoned branch plus two more. The session that will read it ends long before it is asked for.
- **A turn summary**, when the next turn starts. This one runs a model, so it is the only part of the product that spends money.

A result is keyed by the generation it describes. Generations are Merkle ids, so a result computed against a tree that has since moved simply never matches the key a later request builds — a stale answer is unreachable rather than guarded against. Nothing speculative runs on the pipeline thread, and nothing speculative is load-bearing: a full queue, a wedged cache or a missing database all fall through to computing the answer the way it was computed before.

Speculation is **off by default** and configured per developer in `~/.config/acyclic/speculate.toml`, never in the checked-in repo config: whether to spend tokens is a personal decision, not one a teammate inherits from a commit.

```toml
enabled              = true                 # the free half: precompute, zero tokens
kinds                = ["brief", "summary"] # "summary" is the one that spends
command              = ["claude", "-p", "--model", "claude-haiku-4-5-20251001"]
max_runs_per_session = 20                   # hard ceiling
```

**Two gates, not one.** `enabled` alone buys the precompute half. Spending also needs a `command` *and* `"summary"` in `kinds`, so no single boolean can put you on the meter. The command must be on `PATH` or absolute, gets the prompt on stdin, and runs in an empty scratch directory with no filesystem route into the repo — its whole input is a store-computed diff (changed paths and the prompt excerpt, never file contents), so `exclude` governs what it can see. It runs in its own process group, so a timeout kills the children an agent CLI spawns rather than leaking them.

`acyclic summary` and the `summary` MCP tool read what was produced; neither ever produces one on demand, because a request arriving is not consent to spend. `acyclic status` reports the hit rate and what has been spent:

```
speculation:   on — precompute + model runs (claude -p --model claude-haiku-4-5-20251001)
               24h: 31 run(s) · 19 of 31 claimed (61%) · median lead 8.2s · 412.0 KB out
```

Median lead is the number to watch: it is how far ahead of the request a claimed result landed, and near zero means the trigger is firing too late to be worth anything. Design and rationale: [`docs/design/08-speculation.md`](docs/design/08-speculation.md); the egress note is in [`docs/design/07-compliance.md`](docs/design/07-compliance.md).

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

**Local-first, and stronger than "no code leaves the machine":** there is no network code in the product at all. No HTTP client is compiled into any crate, so there is no telemetry, no update check, and no egress to enumerate or firewall. Security review is of a local binary, not a vendor.

Releases carry a SLSA build-provenance attestation and an SPDX SBOM per binary, and every push is scanned for licences, advisories and sources against `deny.toml`. Builds are native per target rather than reproducible, and macOS binaries are not notarized.

**The shipped control on the snapshot store is `exclude`.** Encryption at rest, purge-through-history, secret scanning and enforced retention have been described as controls but are **not built**; purge and GC are blocked on an upstream retention-release fact, so store retention is currently unbounded. [`docs/design/07-compliance.md`](docs/design/07-compliance.md) separates what is true today from what is intended, claim by claim — read it before making a compliance commitment to anyone.

## License

Apache-2.0 (see [LICENSE](LICENSE)). Contributions require DCO sign-off.

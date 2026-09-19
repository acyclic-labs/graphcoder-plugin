# Changelog

All notable changes to this project are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

Pre-1.0; `main` is the only supported line (see `SECURITY.md`).

### Changed

- **The unfinished session-shadowing feature was removed.** It depended on
  replacing the live repository with a native mount, was not wired into host
  approval flows, and could not work consistently across platforms. Existing
  `dry_run` configuration is now rejected; explicit forks remain available.
  Before replacing the old binary, use that binary to stop every daemon with
  an active shadow mount. A new protocol-v2 CLI cannot stop a protocol-v1
  daemon. If the old daemon already crashed, recover the real tree with
  `fusermount3 -u <repo>` (or `fusermount -u <repo>`) on Linux, or
  `umount -f <repo>` (then `diskutil unmount force <repo>` if needed) on macOS.
- **A cold daemon no longer delays the agent's first turn.** The session-start
  hook waits at most 300ms for the daemon; past that it prints a one-line
  notice and returns while the first snapshot builds in the background (251s
  on a 5,000-file tree, previously all of it in front of the first token).
  The daemon binds its socket before it opens the store, so the wait is bounded
  even on an aged store. Tool hooks during the build return in under 100ms.
- **Fork and promote no longer publish inline.** The authority publish is
  O(tree) (7s per fork and 6s per promote on 5,000 files); it now runs on the
  existing idle and every-N timers, like every other checkpoint. A fork is cut
  at its exact base generation, published or not.
- **The daemon exits after an hour idle** with no session or fork
  (`daemon_idle_exit_ms`, 0 disables). Twenty daemons were found alive
  on one machine, eleven for repos that no longer existed.
- `acyclic status` reports the store size from a cache refreshed in the
  background instead of walking the object directory on every call.

### Changed

- **Promote is ~50x faster.** An 8-fork partition round measured 18s per
  promote; it is now 0.15–0.35s, and `acyclic promote <id> <id> ...` lands
  several forks in one call (8 forks in 2.5s). Three causes, three fixes:
  every restored path forced a full-tree rescan because macOS FSEvents
  reports a rename with one path and the watcher treated that as ambiguous
  (fixed in the sdk: a Darwin rename is now a "modified" hint per path, which
  also stops editor atomic saves and `git checkout` from costing a rescan);
  promote landed paths one at a time with a checkpoint pair each (now one
  restore, one row, the written paths captured directly instead of waiting
  for the watcher's echo); and a merged generation was built through the
  store for every promote (now only when a file merged by content or
  conflicted; otherwise the fork's snapshot is the landing source). A
  promote now adds four timeline rows, not twenty.
- The `/fork` skill drops the redundant pre-round checkpoint, skips
  `fork-diff` in PARTITION mode, and promotes every passing fork in one call.

### Added

- **Pydantic AI adapter.** `acyclic install pydantic-ai` writes the AGENTS.md
  cheatsheet and offers (y/N on a terminal, `--yes` for scripts) to add the
  new `acyclic-pydantic-ai` PyPI package to the project (uv, Poetry, a
  requirements file, or a printed `pip install`). The package's `Acyclic`
  capability is one line on an agent, `Agent(model, capabilities=[Acyclic()])`,
  and gives it what the Claude Code hooks give Claude Code: a checkpoint
  before and after every mutating tool call attributed to the tool and the
  `agent.run` turn, the previous session's brief in the instructions, and
  rewind/timeline/diff/restore/turns/brief/checkpoint as native tools. Every
  hook is advisory and bounded; `ACYCLIC_DISABLED=1` turns it off.
  `tests/acceptance/pydantic-e2e.sh` drives a real Pydantic AI agent on a
  scripted model against the daemon on every `run-all.sh` pass.
- `acyclic status` prints a `watcher:` line once the native watcher has lost
  its epoch: invalidation count, full-rescan count and cost, and the last
  reason. `ACYCLIC_TRACE=1` now names the invalidation reason, times each
  restore's phases, each promote's phases, and each baseline's phases.
- `forks.sh` gates a one-path promote at 1.5s and requires no watcher
  invalidation during the round.

## [0.0.2] - 2026-09-17

Everything below shipped in 0.0.2 except Rewind and the npm launcher, which
were 0.0.1. Host coverage, Speculation and Windows are what this release adds.

### Added

- **Rewind** (Launch 1) — Merkle snapshot store with per-host hooks (Claude Code, Codex,
  Cursor, agents-md). Checkpoints every agent action and restores exactly, including untracked
  and gitignored files. Snapshot exclusions, a store-growth proof, license scanning, an attested
  SBOM per binary, `scripts/install.sh`, and a clean-machine install test.
- **Timeline** (Launch 2) — turn-linked metadata index so history can be browsed at any point in
  the conversation, not just by commit.
- **Forks** (Launch 3) — copy-on-write materialization for running parallel attempts, with
  promotion back via three-way merge.
- **Speculation** — the daemon computes what the agent is about to ask for while nobody is
  waiting: the previous-session brief when a session ends, and (optionally, with a model
  command the developer names) a summary of each turn at the turn boundary. Results are keyed
  by the generation they describe, so a tree that moved is a miss rather than a stale answer.
  Off by default, configured per developer in `~/.config/<name>/speculate.toml`, never in the
  checked-in repo config. Adds the `summary` verb and MCP tool, a line in `acyclic status`, and
  a summary line in the session brief. See `docs/design/08-speculation.md`.
- Published to npm as `@acyclic-labs/plugin`.
- **Windows x64 support** — the daemon transport gains a named-pipe implementation alongside the
  Unix domain socket, and host names are encoded per platform (UTF-16LE on Windows) so capture,
  diff, exclusions and guarded fork paths agree with the filesystem. Verified on Windows 11 and
  covered by a `windows-2022` CI job. Forks use full copies there because ProjFS projects a fork
  but does not carry writes back, so a mounted fork would silently lose work. See
  `docs/windows-verification.md`.

### Fixed

- `Config` layering merged per file rather than per key: a checked-in `.acyclic/config.toml`
  silently discarded the whole machine-level layer that `README.md` promises, because parsing
  the repo file started from the defaults. Layers now merge key by key.

### Not yet implemented

- **Monorepo** (Launch 5) — Merkle-aware content and symbol index. Spec only.
- Garbage collection / purge-through-history: the snapshot store is not pruned in v1 by design.
  See "Retention and purge" in `README.md`.

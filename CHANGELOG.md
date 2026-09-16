# Changelog

All notable changes to this project are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

Pre-1.0; `main` is the only supported line (see `SECURITY.md`).

### Added

- **Rewind** (Launch 1) — Merkle snapshot store with per-host hooks (Claude Code, Codex,
  Cursor, agents-md). Checkpoints every agent action and restores exactly, including untracked
  and gitignored files. Snapshot exclusions, a store-growth proof, license scanning, an attested
  SBOM per binary, `scripts/install.sh`, and a clean-machine install test.
- **Timeline** (Launch 2) — turn-linked metadata index so history can be browsed at any point in
  the conversation, not just by commit.
- **Forks** (Launch 3) — copy-on-write materialization for running parallel attempts, with
  promotion back via three-way merge.
- **Safe Mode** (Launch 4) — session redirection and interposition so agent mistakes land in a
  redirected session rather than the working tree; needs the native mount layer.
- **Speculation** — the daemon computes what the agent is about to ask for while nobody is
  waiting: the previous-session brief when a session ends, and (optionally, with a model
  command the developer names) a summary of each turn at the turn boundary. Results are keyed
  by the generation they describe, so a tree that moved is a miss rather than a stale answer.
  Off by default, configured per developer in `~/.config/<name>/speculate.toml`, never in the
  checked-in repo config. Adds the `summary` verb and MCP tool, a line in `acyclic status`, and
  a summary line in the session brief. See `docs/design/08-speculation.md`.
- Published to npm as `@acyclic-labs/plugin`.

### Fixed

- `Config` layering merged per file rather than per key: a checked-in `.acyclic/config.toml`
  silently discarded the whole machine-level layer that `README.md` promises, because parsing
  the repo file started from the defaults. Layers now merge key by key.

### Not yet implemented

- **Monorepo** (Launch 5) — Merkle-aware content and symbol index. Spec only.
- Garbage collection / purge-through-history: the snapshot store is not pruned in v1 by design.
  See "Retention and purge" in `README.md`.

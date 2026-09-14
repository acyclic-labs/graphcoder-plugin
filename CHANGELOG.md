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
- Published to npm as `@acyclic-labs/plugin`.

### Not yet implemented

- **Monorepo** (Launch 5) — Merkle-aware content and symbol index. Spec only.
- Garbage collection / purge-through-history: the snapshot store is not pruned in v1 by design.
  See "Retention and purge" in `README.md`.

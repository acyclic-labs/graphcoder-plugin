# Rewind — implementation plan

Ships Launch 1 (`01-rewind.md`): auto-checkpoint every agent action, `/rewind`, agent self-rollback, blast-radius diff.

The engine is imported, not built: `acyclic-fs` + `acyclic-fs-mount` via path deps on `../fs`. This repo adds the daemon, CLI, metadata index, and the Claude Code adapter. Phase 0 already qualified the engine — verdict in [phase0-verdict.md](phase0-verdict.md).

## The three rules Phase 0 established

1. **Per-tool-call snapshots use `checkpoint()`, never `commit()`.**
   `checkpoint()` is p95 53ms on a 1 GiB tree. `commit()` proves closure over the whole tree (~1.5s/GiB) and runs only at coarse boundaries: session start/end, rewind, every 25 checkpoints, or 60s idle.
2. **The store, socket, and all daemon state live outside the working tree.**
   Capture snapshots everything and fail-closes on sockets — anything we put in-tree breaks checkpointing.
3. **Volumes are configured once, correctly.**
   Raise `maximum_mutations_per_batch` at creation (defaults reject repos >2k files). Persist the `VolumeId` — generations only resolve on their own volume.

## Target layout

```
crates/
  acyclic-engine/       lib: store, pipeline, index, rewind, diff, config
  acyclic/              bin: CLI + hidden `__daemon` subcommand; private protocol module
  acyclic-qual/         Phase 0 harness → standing bench suite (exists)
adapters/claude-code/   hooks, /rewind command, self-rollback skill
tests/acceptance/       one script per acceptance criterion
```

---

# Phase 1 — the engine library (`acyclic-engine`)

**Goal:** everything below the wire works and survives crashes, proven by tests.

### store.rs
- Store root: `~/.local/share/acyclic/stores/<repo-path-hash>/` containing `store/`, `index.db`, `daemon.sock`, `daemon.pid`, `rewind-journal.json`, `trash/`, `meta.json`.
- `meta.json` records `repo_root` and `volume_id`.
- `Engine::init`: create volume (POSIX profile, Durable, batch limits = 4,194,304).
- `Engine::open`: read `meta.json` → `open_volume` → writable Head checkout.
- In-repo `.acyclic/` holds only the checked-in `config.toml`.

### pipeline.rs — the core loop
One task owns the checkout and watcher; requests arrive on a queue; states are `Baselining → Ready → Rewinding`.

- **Startup:** watch → rescan → `capture_baseline` → `checkpoint()` → `commit()` → Ready.
- **Per request (FIFO):** wait for the watcher to go quiet (50ms), `capture_watch_batch`, `checkpoint()`, write index row, reply. Empty batch → `noop` row.
- **Commit cadence:** per rule 1 above; track which generations a commit has published.
- **Failure policy:** a failed capture marks its index row `failed` and the pipeline keeps running. `RescanRequired` → full re-baseline, row marked `recovered`. Commit `Conflict`/`Fenced` should be impossible (single writer) — log, re-checkout, re-baseline.

### index.rs — SQLite, WAL
```sql
sessions(session_id PK, host, started_at, ended_at)
checkpoints(id PK, generation_id UNIQUE, created_at,
            kind: baseline|pre|post|manual|pre_rewind|recovered|failed|noop,
            published,                -- covered by an authority commit yet?
            session_id, tool_call_id, tool_name, label, error, parent_id)
```
`published` exists from day one: unpublished generations are invisible to fs GC, and a future retention story needs to know which is which. Startup reconciles the index against the authority head.

### rewind.rs
Full rewind (pipeline paused):
1. Safety checkpoint (`pre_rewind`) + commit.
2. Read-only checkout of the target → materialize into a sibling temp dir (same filesystem).
3. Write fsync'd intent journal → atomic swap (`RENAME_SWAP` / `RENAME_EXCHANGE`; journaled two-step fallback).
4. Old tree → `trash/` (7-day TTL). Clear journal.
5. Restart watcher + re-baseline (unavoidable: no watcher restart cursor).

Crash at any step: the journal lets startup finish or unwind the swap — the repo is always fully-old or fully-new. The result payload carries a "reload your editor" warning.

Single-file restore (`rewind --path`): copy the one file out of the target checkout in place. No swap, no re-baseline.

### diff.rs
`diff_generations` returns FileIds without paths. Walk the two generations' directory records to build FileId→path maps, join, and emit `path + Added|Removed|Modified|MetadataOnly + sizes`. Cache path maps per generation.

### config.rs
`.acyclic/config.toml` (checked in) + machine defaults. Keys: `quiesce_ms`, `commit_every`, `commit_idle_ms`, `trash_ttl_days`, `store_dir`. Zero config is valid.

**Exit gate:** on macOS + Linux CI — fixture round-trips, pipeline integration tests, and crash-injection tests (helper binary killed at instrumented points: pre/post commit, mid-swap) all green.

---

# Phase 2 — daemon + CLI

**Goal:** the bare-CLI product works end to end.

- **Protocol** (`acyclic/src/proto.rs`): newline-delimited JSON over the local daemon transport. Ops: `ping, status, checkpoint, timeline, rewind, diff, session_start, session_end, commit, stop`.
  The one latency-critical detail: `checkpoint{wait:false}` replies on *enqueue* (~ms — the PostToolUse path); `wait:true` replies when the checkpoint lands (the PreToolUse path).
- **Daemon** (`acyclic __daemon`): socket listener + pipeline + janitor (trash TTL, idle-commit timer). Pidfile prevents doubles; SIGTERM drains the queue and commits.
- **CLI verbs:** `init`, `checkpoint [-m] [--wait] [--durable]`, `timeline`, `rewind <id|--last|--session-start> [--path]` (prints blast summary, confirms), `diff [--stat]`, `status`, `stop`, `install <host>`.
- Interactive verbs autospawn a dead daemon. **Hook-invoked verbs never spawn** — they no-op fast with a warning (exit code 2).

**Exit gate:** the Maya journey from `01-rewind.md` scripted end-to-end on both OSes, and CI asserts warm-daemon `checkpoint` round-trip <100ms p95 on the 1 GiB corpus.

**Status: gate met on macOS** (2026-08-31). `tests/acceptance/`: journey (checkpoints, diff, rewind selectors, cross-session persistence, hook contract), capture-fidelity soak (the test class that caught the FSEvents coalescing bug), crash matrix (kill -9 + mid-swap journal recovery), latency gate (**p95 7.8ms** enqueue-ack round trip on 20k files, budget 100ms). CI workflow runs the suite on macos-14 + ubuntu-24.04; Linux green pending the first CI run (`FS_CHECKOUT_TOKEN` secret required for the sibling fs checkout).

---

# Phase 3 — Claude Code adapter

**Goal:** the same product inside Claude Code. Deliberately thin.

Built as `acyclic hook <event>` + `acyclic install <host>` (the installer embeds the adapter content — no separate `adapters/` dir to drift):

- `acyclic hook pre-tool|post-tool|session-start|session-end` reads the host's JSON payload from stdin (session id, tool name, tool_use_id) and maps to engine ops. Contract: always exit 0, never spawn a daemon, pre-tool waits are bounded (2s deadline), missing daemon is silent.
- `acyclic install claude-code` merges the four hook entries into the repo's `.claude/settings.json` (idempotent, preserves existing settings), and writes `.claude/commands/rewind.md` + `.claude/skills/acyclic-self-rollback/SKILL.md`. Checked-in ⇒ team-shared.
- `acyclic install agents-md` appends the CLI cheatsheet block to `AGENTS.md` for any shell-capable agent.

**Exit gate:** one live session reproducing the full journey — destructive migration, `/rewind` restores gitignored + generated files, agent self-rolls-back mid-task.
**Status: GATE MET (2026-08-31).** `tests/acceptance/claude-e2e.sh` drives a real Claude Code session through the installed adapter (opt-in `ACYCLIC_E2E=1`; skips without the CLI): hooks fire with session + tool attribution, the agent's edit and Bash side effects are checkpointed, blast-radius diff names them, and `rewind --session-start` restores the pre-session tree exactly. The first live run also exposed and fixed a production wedge: watcher invalidation mid-session → recovery baseline hit `DirtyCheckout` (uncommitted overlay) → daemon stuck in `RescanInProgress` permanently. Fixes: publish before any re-baseline, recover with a fresh watcher (a wedged one can never persist), and retry recovery on the next request instead of staying down.

---

# Phase 4 — acceptance + release

**Goal:** provable claims, honest caveats, shippable artifact.

- Acceptance suite: one script per amended criterion — restore fidelity (content + mode; mtimes documented as not restored), checkpoint latency p95, store growth over 100 checkpoints with distinct contents, kill -9 matrix, migration-script scenario.
- Retention policy: **never call `collect_local_garbage`** (it keeps only the head — running it destroys rewind history). `status` reports store size; only trash gets pruned.
- Release engineering: pin the exact `fs` commit for release builds; signed binaries + SBOM per `07-compliance.md`; `install.sh`; README leads with differentiators (captures what Bash did, gitignored state, cross-session) and states the caveats (no exclusions yet, no purge, mtimes, reload-editor).
- Resolve the `acyclic`/graphcoder naming before anything is public.

**Exit gate:** acceptance suite green in CI; clean-machine install flow works.

**Status (2026-09-11): gate met, with the retention/purge scope deliberately narrowed.**

Shipped:
- Acceptance: `exclusions.sh` (declared paths never captured, noop on excluded-only edits, restore refused, rename into an excluded prefix scrubbed, rewind carries the live copies, pre-rule history untouched) and `growth.sh` (60 distinct checkpoints cost ~52 KB each, identical on a 4x larger tree: per-checkpoint overhead is tree pages, never a tree copy). Journey/soak/crash/latency were already green; the migration-script scenario is journey.sh's Bash side-effect step.
- Snapshot exclusions (`exclude` in config): `acyclic-engine/src/exclude.rs`. Watcher hints at or under a rule are dropped before capture; renames across the boundary re-examine the uncovered side; a capture hinted at an ancestor (or the baseline) is followed by a checkout scrub before the generation is checkpointed; a full rewind (and promote / Safe Mode apply, which share `rewind::execute`) moves the live excluded paths into the new tree before the swap, journaled as a `Carrying` phase so kill -9 at any point returns them.
- Retention: `status` reports store size, trash is TTL-pruned. **No fs GC, no purge in v1**, and this is a finding, not an omission: at sdk `8eced48`, `collect_local_garbage` keeps only authority heads plus retention facts (`RetentionKind::{Checkpoint,Pin,ForkBase}`), `retain_workspace_generation` is create-only (no release fact exists), and `prove_generation_closure` does not follow `GenerationRoot::parents`. Running GC would delete every checkpoint but the head; pinning every checkpoint first would make the store append-only forever. Purge cannot remove bytes from a retained generation for the same reason. **Upstream ask:** a retention-release fact (mirror of `encode_workspace_deleted` for retention authorities) honoured by the collector, plus a bulk "retain these N generations" call. With that, the plugin's policy is straightforward: pin what the TTL keeps, release the rest, collect.
- Release engineering: `deny.toml` + a `deny` CI job (licenses inside a permissive allowlist, advisories, sources); an SPDX SBOM generated from `Cargo.lock` per target in `release.yml`, attested against each binary with `actions/attest-sbom`, one copy attached to the GitHub release and listed in `SHA256SUMS`; `scripts/install.sh` (verified download into `~/.local/bin`, `file://` base URL for offline tests); `scripts/install-smoke.sh` runs it on a bare Debian container and drives init → checkpoint → rewind, exclusions included. npm `@acyclic-labs/plugin` 0.0.1 is live.
- Docs: README leads with the differentiators, documents `exclude`, and states the retention stance and caveats.

Still open, by decision: the public name (`acyclic` vs graphcoder), and cutting the first `v*` tag through `release.yml`; both are the owner's call. Exclusions do not reach forks or Safe Mode sessions (they are served from generations), documented as a caveat.

---

# Coordination with `fs` (being fixed upstream)

Two local, uncommitted patches in `../fs/crates/mount` are **release blockers** — fold them in upstream:

| Patch | Where | Why |
|---|---|---|
| Symlink capture metadata | `capture.rs` (`unrestorable_metadata()`) | without it, restore fails on any repo containing a symlink |
| FIFO chmod deadlock | `materialize.rs` + `host_root.rs` (`fchmodat`) | without it, restore hangs forever on a repo containing a pipe |

Nice-to-haves upstream, in order of impact: baseline capture speed (232s/GiB caps `init` UX on big repos), capture-time exclusion rules (secrets), watcher restart cursor (removes the re-baseline tax on daemon restart and after rewind), retention-aware GC, purge-through-history, diff pagination.

# Order of work

store → index → pipeline (long pole) → rewind + diff in parallel → proto/daemon → CLI → adapter. Acceptance scripts accrete per phase, not as a big bang at the end.

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
  acyclic-proto/        lib: CLI ↔ daemon message types
  acyclic/              bin: CLI + hidden `__daemon` subcommand
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

- **Protocol** (`acyclic-proto`): newline-delimited JSON over the unix socket. Ops: `ping, status, checkpoint, timeline, rewind, diff, session_start, session_end, commit, stop`.
  The one latency-critical detail: `checkpoint{wait:false}` replies on *enqueue* (~ms — the PostToolUse path); `wait:true` replies when the checkpoint lands (the PreToolUse path).
- **Daemon** (`acyclic __daemon`): socket listener + pipeline + janitor (trash TTL, idle-commit timer). Pidfile prevents doubles; SIGTERM drains the queue and commits.
- **CLI verbs:** `init`, `checkpoint [-m] [--wait] [--durable]`, `timeline`, `rewind <id|--last|--session-start> [--path]` (prints blast summary, confirms), `diff [--stat]`, `status`, `stop`, `install <host>`.
- Interactive verbs autospawn a dead daemon. **Hook-invoked verbs never spawn** — they no-op fast with a warning (exit code 2).

**Exit gate:** the Maya journey from `01-rewind.md` scripted end-to-end on both OSes, and CI asserts warm-daemon `checkpoint` round-trip <100ms p95 on the 1 GiB corpus.

---

# Phase 3 — Claude Code adapter

**Goal:** the same product inside Claude Code. Deliberately thin.

- `hooks.json`: PreToolUse/PostToolUse on `Edit|Write|MultiEdit|NotebookEdit|Bash` → `acyclic checkpoint …` (always exit 0); SessionStart/End → session markers. *Verify the current hook payload contract against Claude Code docs first — known unverified-host-API risk.*
- `commands/rewind.md`: timeline → pick → confirm → rewind → "reload your editor".
- `skills/self-rollback/`: teaches checkpoint-before-risky-attempt, rewind-on-failure, diff-before-done.
- `acyclic install claude-code` wires it up; `--agents-md` covers every other host.

**Exit gate:** one live session reproducing the full journey — destructive migration, `/rewind` restores gitignored + generated files, agent self-rolls-back mid-task.

---

# Phase 4 — acceptance + release

**Goal:** provable claims, honest caveats, shippable artifact.

- Acceptance suite: one script per amended criterion — restore fidelity (content + mode; mtimes documented as not restored), checkpoint latency p95, store growth over 100 checkpoints with distinct contents, kill -9 matrix, migration-script scenario.
- Retention policy: **never call `collect_local_garbage`** (it keeps only the head — running it destroys rewind history). `status` reports store size; only trash gets pruned.
- Release engineering: pin the exact `fs` commit for release builds; signed binaries + SBOM per `07-compliance.md`; `install.sh`; README leads with differentiators (captures what Bash did, gitignored state, cross-session) and states the caveats (no exclusions yet, no purge, mtimes, reload-editor).
- Resolve the `acyclic`/graphcoder naming before anything is public.

**Exit gate:** acceptance suite green in CI; clean-machine install flow works.

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

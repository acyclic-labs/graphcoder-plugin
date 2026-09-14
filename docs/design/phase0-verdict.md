# Phase 0 verdict — qualifying `acyclic-fs` for the Rewind milestone

> **Historical snapshot.** This records the qualification decision made at a point in time,
> including its dated numbers and status line below. It is not kept in sync with the current
> codebase — check the code and `CHANGELOG.md` for present-day status.

> Produced by the `acyclic-qual` harness (`crates/acyclic-qual`). Machine: macOS (APFS). Status: **GO, with one architecture change and two fs patches.**

## Headline

The imported engine round-trips real repos correctly and can meet the <100ms per-tool-call budget — but **only via `Checkout::checkpoint()` (no authority publish), not `commit()`**. The daemon architecture changes accordingly: cheap `checkpoint()` per tool call (GenerationId recorded in our index), full `commit()` (authority publish, O(tree) closure proof, ~1.5s/GiB) only at coarse boundaries (session start/end, idle, every N checkpoints). Restore from an unpublished checkpoint generation is verified to work, including across a store reopen.

Release-build numbers on a 1 GiB / 20k-file corpus:

| Operation | Latency |
|---|---|
| Watch event arrival (FSEvents → poll) | p50 13ms, p95 13ms (one 423ms outlier) |
| Single-file capture (no publish) | p50 34ms, **p95 37ms** |
| Single-file capture + `checkpoint()` | p50 46ms, **p95 53ms** ✅ inside budget |
| Single-file capture + `commit()` | p95 **1.68s** ❌ O(tree) closure proof |
| Baseline capture (init-time, one-off) | 232s + 3.5s commit — needs upstream work for 10GB `acyclic init` UX (projected ~40min) |

## Integration facts (verified)

- Path imports of `acyclic-fs` (`../fs/crates/sdk`) and `acyclic-fs-mount` (`../fs/crates/mount`) compile from this workspace. macOS needs `PKG_CONFIG_PATH` pointed at `../fs/scripts/pkgconfig/fuse-t` and an rpath to `/usr/local/lib` for `libfuse-t.dylib`; both are encoded in `.cargo/config.toml`.
- Generations are **volume-bound**: `checkout(Exact(gen))` on any other volume fails with `VolumeMismatch`. The daemon must persist `VolumeId` alongside its store and reopen with `open_volume`.
- **Default `VolumeLimits` reject real repos**: `maximum_mutations_per_batch = 2,048` fails any baseline capture beyond ~2k paths ("checkout pending mutation count exceeds the volume limit") — hit at 20k files (synthetic) and ~42k paths (acyclic-website). Limits are immutable per volume, so the daemon must size them at `create_volume` time (harness now uses 4,194,304). Sizing against the largest supported monorepo is a Launch-1 config decision.

## Round-trip verdicts

| Scenario | Verdict |
|---|---|
| Content + mode bits (regular files, exec scripts) | ✅ identical |
| Untracked + gitignored files (`.env`, `.git/`) | ✅ captured and restored |
| Unicode filenames | ✅ |
| Hard links | ✅ |
| Symlinks (valid relative + dangling) | ✅ **after fs patch** — was fail-closed `UnsupportedKind(SymbolicLink)` (bug confirmed exactly as predicted) |
| FIFO | ✅ **after fs patch** — was an unbounded **hang** (worse than predicted abort) |
| 8 MiB multi-chunk binary | ✅ |
| mtime / uid / gid | ❌ not restored (by design in fs materialize) — acceptance criterion amended to content+mode-identical |

## fs patches applied (in `../fs`, uncommitted — need review/upstreaming)

1. `crates/mount/src/capture.rs` — symlinks are captured with fully-`Unavailable` metadata (`unrestorable_metadata()`), and the prior-metadata preserve step is skipped for links. Root cause: capture recorded mode/timestamps that `apply_host_metadata` can never apply to a link, so every tree containing a symlink failed restore fail-closed.
2. `crates/mount/src/materialize.rs` + `crates/mount/src/host_root.rs` — permission application for special files (FIFO etc.) uses a new `HostRoot::set_permissions_without_open` (`fchmodat`) instead of cap-std's open-based `set_permissions`. Opening a FIFO for chmod blocks until a peer appears — materialize of any tree containing a FIFO deadlocked forever.

Both patches pass the fs repo's own suites: `acyclic-fs` 460/460, `acyclic-fs-mount` 18/18. (Both were subsequently folded into the fs branch.)

3. `crates/sdk/src/watch.rs` (+ its test) — **silent stale-content capture on macOS**, found by a 60-cycle CLI soak: FSEvents coalesces per-path flags, so notify can surface a data write as `Modify(Metadata)`; fs mapped that to `MetadataChanged`, whose capture intent deliberately skips restaging content. Under rapid same-file edits this froze captured content permanently while checkpoints kept succeeding (worst failure class: silent corruption of history). Fix mirrors fs's existing Darwin `Create` downgrade: on macOS a metadata hint degrades to `Modified`. Soak: 30/60 checkpoints wrong before, 0/60 after (both `--wait` and queued modes); fs suites 573+21 green.

## Fixture round-trip timings (20 paths, ~8.4 MiB, debug build)

- capture_baseline ≈ 1.22 s (dominated by fixed per-capture overhead, see bench)
- commit ≈ 13 ms; materialize ≈ 220 ms; compare ≈ 5 ms

## Real repo (acyclic-website: 335 MB, node_modules installed, 8,459 paths, 16 symlinks)

✅ **Round-trip content + mode identical.** Debug-build timings (upper bounds): baseline capture 494s, commit 10.7s, materialize 124s.

## Watch-driven incremental capture (1 GiB / 20k-file corpus, debug build)

- Event-arrival latency (FSEvents → poll): **p50 5.4ms, p95 13.2ms** — comfortably inside the hook budget; `quiesce_ms ≈ 50` looks right.
- Single-file capture+commit: **p95 ≈ 2.57s (debug)** — far over budget; release-build split (capture vs commit) pending to attribute. If release commit stays ~seconds, per-checkpoint cost scales with tree size, not delta size — upstream conversation required.
- Baseline capture 434s + commit 7.6s (debug).

## Launch 3 mount gate (F0, 2026-08-31)

✅ **GO on native mounts for the fork engine** (`acyclic-qual mount-smoke`, macOS FUSE-T via the fs C bridge): probe reports available+writable; mount of a captured checkout attaches in **86ms**; first small read 2.6ms; 8MB lazy hydration 38ms; symlink targets exact; overlay writes/edits visible through the mount and fully isolated from the source tree; clean detach. Reflink fallback not needed.

## Crash and growth checks

- ✅ **kill -9 mid-baseline-capture**: store reopens clean; subsequent capture + restore in the same store succeeds.
- ✅ **Store dedup**: 1.1 GiB corpus + 30 checkpoints → 85 MB store (corpus is degenerate — identical file contents — so this proves dedup works, not realistic growth). Real repo: 335 MB tree → 434 MB store for one baseline (~1.3x; object encoding overhead).
- ✅ **Restore from unpublished `checkpoint()` generation**: content+mode identical, across store reopen (12.6ms checkpoint vs 1s+ commit on the fixture).

## Remaining (Phase 1 scope, automated in CI)

- [ ] 10 GiB corpus (projected from 1 GiB: baseline ~40min — flag for upstream before promising 10GB init UX)
- [ ] kill -9 injected precisely during commit and during materialize (needs instrumented injection points)
- [ ] Realistic store-growth curve (distinct file contents, 100-checkpoint session)
- [ ] Caveat for daemon design: authority-rooted GC does not know about checkpoint-only generations — never GC while unpublished checkpoints matter (v1 policy is never-GC anyway).

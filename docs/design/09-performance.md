# 09 · Performance: closing the O(tree) gaps

Status: items 1, 5 (plugin half) and 7 (idle exit, status) shipped
2026-09-17; the rest are open decisions, tracked in the performance
decisions artifact. Measured 2026-09-17 on main 333001e.

## What is slow, and why

Everything that happens per turn is fine at any size: warm session start,
user-prompt and post-tool hooks are ~40ms, pre-tool ~100–180ms. Every
operation that touches the store is O(tree), and the per-file unit of cost
is a barrier fsync, so it is also O(history) as the store ages.

| op | 100 files | 1,000 | 5,000 | what it is |
|---|---|---|---|---|
| cold session start | 4.3s | 18–72s | 251s | store open + full baseline; blocks first token |
| rewind | 4.0s | 38s | 211s | root swap + full baseline, then a second rescan |
| fork -n 1 (first) | 0.13s | 1.7s | 7.0s | authority publish |
| promote, 1 file | 0.5s | 1.3s | 11.1s | merge_plan (3 tree walks) + publish + capture |
| brief / diff / fork-diff | 60ms | 0.2–1.5s | 3–6s | full-tree walks in `diff.rs` |
| direct capture, 1 file | 0.25s | 0.25s | 0.25s | ~10 fsync'd object appends |

Aged store (118-file demo, one day of rounds, 30k objects): store open
10.4s, baseline 7.6s (1.2s fresh), rewind 8.2s, `status` 0.9s.

Six mechanisms, in order of cost:

1. **Full baseline.** `capture_baseline` re-stages every file through the
   object store on every daemon start and after every rewind, at 12–48ms
   per file. Nothing is lazy; the SessionStart hook blocks on it.
2. **Authority publish is O(tree) and inline.** `commit_engine` runs a
   closure proof over the whole tree on every fork cut and every promote.
3. **Store diffs walk the whole tree.** `diff::summaries` lists every
   directory of both generations. The sdk's `diff_generations` skips
   equal subtrees but returns `FileId`s keyed by parent identity, not
   paths, so the plugin does not use it.
4. **Object store open replays history.** Journal replay plus an open and
   hash of every live manifest; grows every round; nothing compacts it.
5. **A fsync per object append** (`LocalObjectsDurability::Barrier`; the
   only alternative is `FullFlush`, which is worse).
6. **Daemons never exit**, one per repo, including repos that no longer
   exist.

## The plan

Ordered by payoff per unit of work. P = plugin, S = sdk.

### 1. Never block the first token (P, small)

The SessionStart hook spawns the daemon and returns as soon as the socket
answers, printing the brief from `index.db` directly. If the daemon is
still baselining it says so in one line. The first pre-tool hook already
bounds its wait at 2s; a post-tool enqueues. Nothing else changes.

Open: whether the hook should wait a short bounded time (say 300ms) so a
warm-ish start still prints a complete brief, or never wait.

### 2. Incremental baseline (S, medium)

On start, if the store has a head generation: walk the host tree collecting
(path, kind, size, mtime, ctime), compare against the head's file records
(`FileMetadata` carries `modified_ns`, `changed_ns`; size is on the record),
and capture only paths that differ or are new, plus removals for records
whose path is gone. Fall back to the full baseline when there is no head,
when more than N% of paths differ, or when the root identity changed.

Open: trust mtime+size+ctime, or also hash small files; and what the
fallback threshold is.

### 3. Batch durability (S, medium)

Add `LocalDurability::Deferred`: appends are written without a barrier;
one barrier is issued at checkpoint end and at commit. A crash loses at
most the checkpoint in flight, which is the guarantee the store already
claims ("a torn tail just drops the newest checkpoint").

### 4. Journal snapshot on open, and GC (S, medium-large)

`LocalObjects` writes a snapshot of its live state at commit (or every N
operations); open loads the snapshot and replays only the tail. GC
(`collect_garbage` exists) runs on the idle timer once retention is
settled. Store open becomes O(live objects) then O(1).

### 5. Publish off the critical path (P then S)

P: fork and promote stop calling `commit_engine` inline; the idle commit
timer (already 60s) publishes. A fork's base and a promote's landed row are
recorded as unpublished, which the timeline already displays. Risk: a
daemon crash between checkpoint and publish loses the unpublished rows,
which is the same window every other checkpoint already has.
S, later: incremental closure proof so publish itself is O(changes).

### 6. O(changes) diffs (P+S, medium)

A pruned walk over the namespace trees does NOT work: a directory's
entries tree identifies names and file ids, and a file id is stable across
content edits, so an edited file leaves every ancestor's entries tree
unchanged (tried 2026-09-17; four merge tests caught it). The Merkle that
changes is the file table, which `diff_generations` walks, returning file
ids without paths. Options: an id→path map for the head kept current from
binding changes (falls back to the walk for historical pairs), or the sdk
adding parent identity to records or a path-resolution call.

### 7. Housekeeping (P, small)

Daemon exits after an idle period with no sessions (keeping mounted forks
alive means: no exit while forks are live). `status` stops walking the
object directory. Rewind should not force a second rescan: re-baseline once
with the new root identity, or better, land a rewind as a restore of the
changed paths rather than a root swap when the diff is small.

## Measurements

Source: `matrix2.sh` (synthetic 1KB files, 50 per directory, fresh store per
size, `ACYCLIC_TRACE=1`), and `hooks.sh`, both in the session scratchpad;
runner outputs are in the PR #17 description and the memory notes.
Run-to-run variance is ~2x because the unit of cost is fsync latency.

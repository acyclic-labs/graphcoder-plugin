# Fork engine — behavioral spec and verification matrix

The contract `acyclic fork / forks / fork-drop / promote` must satisfy, and
the tests that prove each clause. Written while the FUSE-T multi-session
fixes land: the **P-series** (promote logic) is mount-independent and runs
now; the **M-series** (mount behavior) runs the moment mounts are back.

## Definitions

- **Mainline** — the real working tree + the pipeline's writable Head
  checkout.
- **Fork base `B`** — the published generation at fork creation. `fork`
  publishes pending state first, so every fork in one `fork -n N` call
  shares one base.
- **Fork workspace** — a routed subdirectory `<repo-parent>/.<repo>.forks/mnt/<id>/`
  of ONE shared native session (fs `RoutedMountSource`), backed by a fresh
  Head checkout; writes accumulate in that checkout's private overlay. One
  kernel mount total, regardless of N — forks are route inserts.
- **Moved mainline** — a *published generation* other than `B` exists at
  promote time. Publishing identical content (noop commits) does not move
  the mainline: movement is judged by fs's optimistic commit against the
  authority head, which only advances on real published change.

## Invariants

- **I1 (isolation-out):** nothing written in a fork is observable in the
  mainline tree or store head until that fork is promoted.
- **I2 (isolation-across):** nothing written in fork X is observable in
  fork Y.
- **I3 (mainline-usable):** checkpoints, diffs, and rewinds on the mainline
  proceed normally while forks are live.
- **I4 (no-silent-corruption):** promote either lands exactly the fork's
  tree, reports a legible conflict leaving the tree untouched, or fails
  with an error leaving the tree untouched. No fourth outcome.
- **I5 (evaporation):** dropping a fork (or stopping the daemon) leaves no
  serving route, no mount (once the last route goes), and no store garbage
  a later GC story couldn't collect; the mainline is byte-unchanged.
  Caveat: the kernel may cache a dropped route's NAME as a phantom empty
  directory until it re-validates (FSKit holds positives); the route serves
  nothing and is unlisted — verified as `route_dead` in forks.sh.
- **I6 (attribution):** every promote is a timeline row; a rewind can undo
  a promote like any other change.
- **I7 (identity):** fork ids never collide with a live fork; workspace
  dirs are outside the repo (capture never sees them).

## Verb contracts

### `fork -n N` (1 ≤ N ≤ 16)
1. Publishes pending mainline state; records a `manual` "fork base" row.
2. Creates N writable Head checkouts, mounts each at a fresh empty
   workspace dir, returns ids + paths.
3. Cost: O(1) in repo size (no copying); creation < 1s per fork on the
   fixture repo. Partial failure (mount K fails) leaves forks 1..K-1 live
   and reports the error.

### `forks`
Lists live forks (id, age, path, base prefix). After a daemon restart the
list is empty and says forks don't survive restarts (v1).

### `fork-drop <id>`
Unmounts, discards the overlay, removes the workspace dir. Unknown id → error.

### `promote <id>`
1. Unmounts the fork first (no writes can race the commit).
2. A rebased fork (see 4) still holding conflict markers in any conflicted
   path → refused by name; nothing changes.
3. Fork with no writes → success, "nothing to land", mainline untouched.
4. Mainline moved (per Definitions) → three-way merge of base/head/fork
   per `plans/implementation-merge.md`: fork-only paths are replayed in
   place, files both sides edited are merged by line, and the result is
   checkpointed and published ("promoted by merge/replay"). A content
   conflict rebases the fork onto the head, writes diff3 markers into the
   fork, and lands nothing; the fork stays live for resolve-and-promote.
   A refusal (binary, too large, kind change, directory ancestry) is a
   legible error naming the paths; fork and tree untouched.
5. Otherwise (mainline unmoved) → the fork's changed paths are written
   onto the real tree one at a time with the same atomic single-path
   restore, checkpointed and published; a `manual` row labeled
   `promote <id> (N path(s) written in place)` records it. The repo
   directory is never replaced by a promote (Safe Mode `session-apply`
   still swaps and says so). Gitignored files included.

### Daemon stop / crash
Stop unmounts all forks and removes workspace dirs before the pipeline
shuts down. A crashed daemon's stale workspaces are swept (unmount +
remove) at next start, before the store opens.

## Verification matrix

| Clause | Test | Layer | Mount-free? |
|---|---|---|---|
| P1: promote lands fork content exactly | `tests/fork.rs::promote_lands_fork_changes` — fork seed, SDK `create_file`/`write_file` into overlay, promote, byte-verify tree + timeline row | engine integration | ✅ |
| P2: no-write promote is a no-op | `tests/fork.rs::promote_of_untouched_fork_is_noop` | engine integration | ✅ |
| P3: moved mainline → legible conflict, tree untouched | `tests/fork.rs::promote_conflicts_when_mainline_moved` — real edit + durable checkpoint between fork and promote | engine integration | ✅ |
| P4: noop mainline publishes don't fake conflicts | `tests/fork.rs::noop_commits_do_not_move_mainline` — `commit` with no changes between fork and promote, promote succeeds | engine integration | ✅ |
| P5: fork base rows recorded; promote row recorded (I6) | asserted inside P1 | engine integration | ✅ |
| M1: `fork -n 3` yields 3 routed workspaces under ONE mount, fast (I7) | `forks.sh` step 1 (+ `mount` table count == 1) | acceptance | ❌ |
| M2: divergent writes isolated (I1, I2) | `forks.sh` step 2 — different content per fork, mainline + siblings unchanged | acceptance | ❌ |
| M3: mainline usable while forks live (I3) | `forks.sh` step 3 — checkpoint + diff succeed with mounts up | acceptance | ❌ |
| M4: promote journey end-to-end (I4, I6) | `forks.sh` step 4 — promote A, byte-verify, losers dropped, timeline row | acceptance | ❌ |
| M5: moved mainline via CLI (I4) | `merge.sh` G1–G26 — replay, content merge, conflict rebase + resolve, refusals, undo, CRLF, all-or-nothing; mount and copy modes | acceptance | ❌ |
| C1: entry table, merge3, markers, text gate | `merge.rs` unit fixtures (graphcoder's six merge3 tests included) | unit | ✅ |
| C2: plan over real generations, M and R, rebase into an overlay | `tests/merge.rs` | engine integration | ✅ |
| M6: evaporation + sweep (I5) | `forks.sh` step 6 — fork-drop leaves nothing; kill -9 daemon, restart sweeps workspaces | acceptance | ❌ |
| M7: stop cleans up (I5) | `forks.sh` teardown asserts no `forks` mounts remain | acceptance | ❌ |
| U1: fork ids random-tailed, no collision (I7) | `server.rs` short_id guard + `fork.rs::forks_root_is_a_hidden_sibling` | unit | ✅ |
| U2: stale sweep removes dirs | `fork.rs::sweep_removes_stale_directories` | unit | ✅ |

Out of scope (documented): rename detection, semantic merges, conflict
markers on the mainline, Safe Mode `apply_session` onto a moved mainline,
fork persistence across daemon restarts, subagent orchestration, per-fork
port/env provisioning.

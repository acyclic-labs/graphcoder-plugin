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
  mount, no workspace directory, and no store garbage that a later GC
  story couldn't collect; the mainline is byte-unchanged.
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
2. Fork with no writes → success, "nothing to land", mainline untouched.
3. Mainline moved (per Definitions) → **conflict**: legible message naming
   the base, tree untouched, fork consumed (v1; re-fork to retry).
4. Otherwise → fork overlay publishes (head `H2`); mainline tree is
   replaced with `H2` via the journaled rewind swap (safety `pre_rewind`
   row first); a `manual` row labeled `promote <id>` records it; watcher
   re-baselines. Byte-verified content, gitignored files included; the
   same mtime/editor caveats as rewind.

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
| M5: conflict via CLI (I4) | `forks.sh` step 5 — durable checkpoint then promote → error text, tree untouched | acceptance | ❌ |
| M6: evaporation + sweep (I5) | `forks.sh` step 6 — fork-drop leaves nothing; kill -9 daemon, restart sweeps workspaces | acceptance | ❌ |
| M7: stop cleans up (I5) | `forks.sh` teardown asserts no `forks` mounts remain | acceptance | ❌ |
| U1: fork ids random-tailed, no collision (I7) | `server.rs` short_id guard + `fork.rs::forks_root_is_a_hidden_sibling` | unit | ✅ |
| U2: stale sweep removes dirs | `fork.rs::sweep_removes_stale_directories` | unit | ✅ |

Out of scope (v2 of this launch, documented): three-way merge onto a moved
mainline, fork persistence across daemon restarts, subagent orchestration,
per-fork port/env provisioning.

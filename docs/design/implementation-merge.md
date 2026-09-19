# Merge v2: content-level merges — implementation plan

Extends the merge primitive shipped in 832e975 ("promote replays a
path-disjoint fork onto a moved mainline"). Today, if the mainline and a
fork both changed the same file, promote refuses and names the file, even
when the two sides edited different lines (`merge.sh` G4 asserts this on
purpose). This plan makes that case land and aligns the rest of promote
with the merge semantics graphcoder already defines (see the appendix,
which maps every rule here to its graphcoder source).

## Scope

**In:** the graphcoder merge model, applied to `acyclic promote`:

1. Entry-level three-way merge of the two trees (base B, mainline H,
   fork F), with graphcoder's `mergeTrees` decision table.
2. Line-level three-way merge of regular text files that both sides
   changed, with graphcoder's `merge3` rules: diffy, diff3-style markers
   that include the base, the trailing-newline rule, side labels.
3. On a content conflict the fork is **rebased onto H** and the
   conflicting files are written into the fork workspace with markers.
   The mainline is untouched. The agent resolves the markers in the fork
   and promotes again. This replaces the v1 "refuse and discard the fork"
   policy from c905be4.
4. Everything else in the promote path (safety checkpoint, path-by-path
   atomic replay, timeline rows, rewind undo, mount and copy modes) is
   reused unchanged.

**Status (2026-09-10):** implemented. Phases C0–C3 landed together; the
"as built" notes at the end record where the build deviated from this
plan.

**Out (later launches, listed so nobody re-litigates them mid-build):**
rename detection; semantic or AST merges; markers written into the
*mainline*; merging directory-ancestry overlaps; live rebase
of a fork while the mainline moves.

## Definitions

- **B** — the fork base generation. **H** — the published head at promote
  time (the mainline). **F** — the fork's overlay snapshot. All three are
  generations the engine can `checkout_exact`.
- **Sides.** The fork is **ours**, the mainline is **theirs**. This is
  graphcoder's convention (child merges *into* parent; child = ours) and
  the opposite of git's. Marker labels: `<<<<<<< fork <id>` and
  `>>>>>>> mainline`.
- **Fork paths / head paths** — `content_changes(diff(B, F))` and
  `content_changes(diff(B, H))`, exactly as `replay_onto_head` computes
  them now.
- **Merge generation M** — H plus the merged entries: the fork's
  cleanly-taken paths and the content-merged files. Replay writes the
  fork's paths from M onto the real tree. This is graphcoder's
  `applyPageTreeEntries(parentTree, mergedEntries)` in engine terms.

## Invariants (adds to spec-forks.md I1–I7)

- **I8 (no silent content loss):** a merged file is exactly what diff3 of
  (B, ours=F, theirs=H) produces. Anything that cannot be established
  byte-for-byte is a conflict or a refusal, never a guess.
- **I9 (all-or-nothing):** conflicts and refusals are collected across
  every path before anything is written. A promote with one clean file
  and one conflicting file lands nothing on the mainline. Graphcoder's
  `mergeTrees` returns the full conflict list the same way.
- **I10 (mainline untouched on conflict):** a conflict changes only the
  fork workspace. The real tree and the head are byte-identical before
  and after.
- **I11 (attribution):** M is a recorded generation; a rebase of the fork
  records a `fork <id> rebased onto <H>` row. `rewind --last` after a
  landed merge returns to the pre-merge safety row exactly as after a
  replay.

## Entry-level decision table

Computed for every path in `fork_paths ∪ head_paths`, in sorted order.
Entries compare by kind and payload object id as `diff.rs` does today.

| Condition | Outcome | graphcoder |
|---|---|---|
| F entry == H entry | take (either) | `sameEntry(left,right)` |
| H entry == B entry | take F | `sameEntry(left,base)` |
| F entry == B entry | take H | `sameEntry(right,base)` |
| B absent or dir, H dir, F dir | **descend**: not a conflict; resolved by the paths beneath | independent-tree overlap expansion |
| B regular, H regular, F regular, both text | **content merge** (next section) | `merge3` |
| B present, one side removed, other modified | **content conflict**, deleted side empty, label suffix `(deleted)` | marker variant `<<<<<<< id (deleted)` |
| B absent, both added, differ, both text | **content merge with empty base** (conflicts unless identical) | `merge3("", ours, theirs)` |
| kind changed on either side (file↔symlink↔dir) | **refuse** | tree-level conflict, blocked |
| any side fails the text gate | **refuse** | `merge3` is string-only |

The **descend** row is a change from v1: today a fork adding
`docs/new/a.md` while the mainline adds `docs/new/b.md` is refused
because `docs/new` appears on both sides. Under this table both land.
Delete-a-directory versus add-inside-it (G5/G6) stays a conflict: the
deleted side is absent, not a directory, so the descend row does not
apply, and it is not a regular file either, so it refuses.

**Text gate:** valid UTF-8 on all three sides and size at most
`[merge] max_file_bytes` (default 4 MiB). Graphcoder's `merge3` takes
JS strings, so non-UTF-8 content never reaches it; the plugin makes that
explicit. A NUL byte fails the gate as well.

**Metadata:** mode-only changes are ignored in the overlap check (v1
decision in c905be4, kept). When a file is content-merged, M carries the
mode of the side that changed it, H if both did. Graphcoder compares
whole entries and has no mode merge; this is a deliberate divergence,
recorded in the appendix.

## Content merge (`merge3` rules)

Given base, ours (F), theirs (H):

1. Normalize each input to end with `\n` if non-empty and missing one.
2. `diffy` merge with `ConflictStyle::Diff3` (the base block is included
   between `|||||||` and `=======`) and marker length 7.
3. Clean: if `merged_has_trailing_newline(base, ours, theirs)` is false,
   drop the final `\n`. The rule: if ours and theirs agree on having a
   trailing newline, use that; else if ours agrees with base, use
   theirs'; else use ours'. Port the function verbatim from
   `lib/compute/src/merge.rs`.
4. Conflict: replace `<<<<<<< ours` with `<<<<<<< fork <id>` and
   `>>>>>>> theirs` with `>>>>>>> mainline`. For modify/delete the label
   of the deleting side gets ` (deleted)` and the other ` (modified)`.
5. Line endings: diffy splits on `\n`; a CRLF file keeps its `\r` as line
   content, so CRLF survives a clean merge. Fixtures assert this.

Outcome per file: `Merged(bytes)` or `Conflicted(bytes_with_markers,
hunks)`. Both are computed before any write.

## Conflict → rebase the fork

When at least one file is `Conflicted` and nothing is refused:

1. Build **R** = M with the conflicted files replaced by their
   marker-bearing content. R is F rebased onto H.
2. Write R's differences from F through the mounted workspace so the mount
   and kernel caches observe every change.
3. Set `fork.base = H`. Record R as `fork <id> rebased onto <H-hex>
   (N conflict(s))`.
4. Record the open conflict on the fork: `{ base: B, ours: F, theirs: H,
   paths }`. This is graphcoder's `FS_CONFLICT_OPENED` payload. `acyclic
   forks` shows `conflict: N path(s)` for that fork.
5. Return the refusal below. The mainline and head are untouched (I10).

```
promote fork abc: 2 file(s) conflict; markers written into the fork
  src/auth.js: 3 conflicting hunk(s)
  src/shared.js: mainline deleted, fork modified
Resolve the markers in /path/.repo.forks/mnt/abc and run `acyclic promote abc` again
```

**Re-promote.** The fork's base is now H. Before merging, the engine
scans every path recorded on the open conflict in F: a file that still
matches `^<<<<<<< ` and a `^=======$` line refuses with `unresolved
markers in: <paths>`. A deleted conflicted path counts as resolved
(agent chose the deletion). Then promote proceeds normally: if the
mainline is still at H the fork lands by swap or replay; if it moved
again, the merge runs again against the new head and may conflict again.
That is graphcoder's "resolve conflict markers and call merge again".

**Refusals** (kind change, text gate) are different: nothing is written
anywhere, the fork keeps its base, and the message names the path and
reason. Graphcoder reports these as `blocked` with paths and does not
alter the child. The v1 fork-discard on refusal is dropped for
consistency: the fork survives every non-landing outcome.

## Where the code goes

```
crates/acyclic-engine/src/merge.rs        NEW — entry table, merge3 port, marker scan
crates/acyclic-engine/src/pipeline.rs     BuildGeneration { from, entries } request
crates/acyclic-engine/src/fork.rs         ForkSeed.base becomes mutable; OpenConflict
crates/acyclic-engine/src/config.rs       [merge] max_file_bytes
crates/acyclic/src/server.rs              replay_onto_head → merge_onto_head; rebase path
crates/acyclic-proto/src/lib.rs           PromoteInfo.merged_files; ForkInfo.conflict
crates/acyclic/src/main.rs                promote / forks output
crates/acyclic/src/install.rs             skill: PARTITION + conflict resolution
tests/acceptance/merge.sh                 G4/G5-adjacent flips; G13–G26 added
```

### Engine: `merge.rs`

```rust
pub enum EntryMerge { Take(Side), Descend, Content(ContentMerge), Refuse(Reason) }
pub enum ContentMerge { Merged(Vec<u8>), Conflicted { bytes: Vec<u8>, hunks: u32 } }
pub fn merge_entry(base: Option<&Node>, ours: Option<&Node>, theirs: Option<&Node>,
                   limits: &MergeLimits) -> EntryMerge;
pub fn merge3(base: &str, ours: &str, theirs: &str, ours_name: &str, theirs_name: &str)
    -> Merge3Result;                              // verbatim port
pub fn has_conflict_markers(text: &str) -> bool;  // verbatim port of markers.ts
pub struct MergePlan { pub merged: Vec<(PathBuf, Node)>, pub conflicted: Vec<(PathBuf, Node, u32)>,
                       pub refusals: Vec<(PathBuf, Reason)> }
pub async fn plan(store, base, head, fork, limits) -> Result<MergePlan>;
```

`merge_entry` and `merge3` are synchronous and take bytes, so the tables
above are unit-testable without a store. `plan` opens three exact
checkouts, walks the union of changed paths (descending where the table
says so), reads regular-file bodies via `read_file_range` (the
`write_node` loop in `rewind.rs` is the pattern), and returns everything
before anything is written.

### Engine: build M and R

One pipeline request `BuildGeneration { from: GenerationId, entries }`:
scratch checkout at `from` (`scratch_checkout` exists), write each entry
through the SDK checkout API the fork tests already use, `checkpoint()`
it unpublished, then `record_generation` it.
M = `BuildGeneration(H, merged ∪ taken)`; R = `BuildGeneration(M,
conflicted)`. No head movement, no tree writes.

### Daemon: `merge_onto_head` (renames `replay_onto_head`)

1. Fork paths, head paths — unchanged.
2. `merge::plan(B, H, F)`.
3. Any refusal → error, nothing written, fork unchanged.
4. Any conflict → build R, write R−F into the fork, `fork.base = H`,
   record the open conflict, return the conflict message.
5. Otherwise build M, then the existing tail: safety checkpoint, replay
   the fork's paths from M, checkpoint, commit. Landed row label gains
   `merged N file(s)`.

### Wire and CLI

`PromoteInfo.merged_files: u32` (default 0). `ForkInfo.conflict:
Option<{ paths: u32, base, ours, theirs }>`. CLI:

```
promoted by merge: 1 file(s) merged, 3 path(s) written in place, tree now at <hex>
```

`acyclic forks` appends `conflict: 2 path(s), resolve then promote` on
a rebased fork.

### Skill (`install.rs`)

- PARTITION: "Independent parts that share a file" drops from "SEQUENCE
  the shared file first" to "fine if the parts touch different regions of
  it; SEQUENCE first only for edits others depend on (a type, an
  interface, a config key)".
- Troubleshooting replaces "the conflicting fork is discarded; do not try
  to salvage it" with the graphcoder rule: a promote that reports
  conflicts has written markers into that fork; open each named file in
  the fork, resolve every `<<<<<<<`/`|||||||`/`=======`/`>>>>>>>` block
  (the middle block is the base), leave no markers, and promote again. A
  `refused` promote (kind change, binary, too large) cannot be resolved
  in place: one fork must own that path; re-fork and redo that part.

## Phases

**C0 — merge core (engine, no I/O).** `merge.rs` with `merge_entry`,
the verbatim `merge3` and `has_conflict_markers` ports, the text gate,
and the fixture set. Gate: every fixture that exists in graphcoder's
`merge.rs` tests passes byte-identically here.

**C1 — plan and generations.** `merge::plan`, `BuildGeneration`,
config knob. Engine integration test builds B/H/F through scratch
checkouts and asserts M's and R's content and that no head moved.

**C2 — daemon and surfaces.** `merge_onto_head`, fork rebase, open
conflict state, proto fields, CLI lines, marker-scan on re-promote.
Skill text update.

**C3 — acceptance.** `merge.sh` changes below, run in mount mode on macOS
and copy mode on Linux, plus `forks.sh` and the journey
suites unchanged. Update `spec-forks.md`'s out-of-scope line, its stale
M4/M5 rows, and the c905be4 "discard" wording in the same commit.

## Verification matrix

Unit fixtures (C0). The first six are graphcoder's own `merge3` tests.

| Case | Expect |
|---|---|
| clean, no overlap (ours top, theirs bottom) | clean, both present |
| same line both sides | conflict, `<<<<<<< fork x` / `>>>>>>> mainline` |
| identical change both sides | clean |
| no trailing newline anywhere, conflict | markers each on their own line |
| base "" ours "added" theirs "" | clean, `added` with no newline appended |
| base "" both add differently | conflict |
| trailing newline: ours+theirs agree / ours=base / neither | rule applied exactly |
| CRLF both sides edit | clean, CRLF preserved |
| modify/delete, delete/modify | conflict, `(deleted)` / `(modified)` labels |
| dir/dir with base absent | Descend |
| file→symlink one side | Refuse KindChange |
| invalid UTF-8 or NUL on any side | Refuse Binary |
| size > max_file_bytes | Refuse TooLarge |
| `has_conflict_markers` on resolved, partially resolved, `=======` alone, marker not at line start | matches markers.ts |

Acceptance (`merge.sh`, each from a fresh fork set):

| Id | Case | Expect |
|---|---|---|
| G4 (flipped) | different lines of one file | lands; both edits; "promoted by merge: 1 file(s)" |
| G13 | same line both sides | refused with `1 conflicting hunk(s)`; mainline untouched; fork alive; fork's file has diff3 markers with the base block |
| G14 | resolve G13's markers in the fork, promote again | lands; resolved content; conflict cleared in `forks` |
| G15 | promote again with markers still present | refused `unresolved markers in: src/shared.js`; nothing changes |
| G16 | mainline moves again between G13 and G14 | second merge against the new head; lands or conflicts again legibly |
| G17 | identical change both sides | lands as replay, 0 merged |
| G18 | modify/delete | conflict with `(deleted)` label in the fork; deleting the file in the fork then promoting lands the deletion |
| G19 | add/add differing | conflict with empty base block |
| G20 | both sides add different files under one new directory | lands (descend) — v1 refused this |
| G21 | binary on one side | refused with `binary`; fork alive and unchanged |
| G22 | three forks all edit `src/shared.js` in disjoint regions, promoted in sequence | all three land; third merges against the merged result |
| G23 | merge + disjoint paths in one promote | both counts printed; all paths correct |
| G24 | rewind to the `before promote … (merge)` row after a landed merge | tree back at the pre-merge file (`--last` targets the landed row itself, by design) |
| G25 | CRLF file merged through the CLI | bytes unchanged apart from the two edits |
| G26 | one mergeable and one conflicting file in one fork | nothing lands; only the conflicting file gets markers; the mergeable one is merged in the fork (it is part of R) |

## Risks and open questions

- **Writing into a live mount fork.** R−F goes through the mounted path while
  the agent may hold it open. The driver serializes the resulting overlay
  mutations; an editor with the file open sees markers on its next read.
- **Fork base mutation.** `ForkSeed.base` is immutable today and the
  daemon's fork table copies it. Both must update atomically with the
  rebase record; a daemon restart already loses forks, so no persistence
  work is needed.
- **diffy fidelity on CRLF.** Covered by fixtures; the port of the
  trailing-newline rule is what graphcoder ships, so behavior stays
  aligned even where diffy is quirky.
- **Large overlap sets.** `plan` reads every overlapping file fully from
  three generations. Bounded by `max_file_bytes` times overlaps.
- **Policy reversal.** Dropping the c905be4 fork-discard is the one place
  this plan changes something already shipped and documented in the
  skill. It is required for the resolve-and-retry loop and matches
  graphcoder; the skill text carries the new rule.

## Appendix: source of each rule in `../graphcoder`

| Rule here | graphcoder source | Status there |
|---|---|---|
| Entry table (take / take / take / descend / conflict) | `lib/fs/src/kernel/merge.ts` `mergeTrees`, `expandIndependentTreeOverlaps` | live, used by the `merge` tool (`brain/.../tools/routing/merge.ts`) |
| Result = parent tree + merged entries | same file, `applyPageTreeEntries(parentTreeHash, entries)` | live |
| Conflicts collected in full, no partial landing | same file, `conflicts` list returned as `status: "conflict"` | live |
| ours = child/fork, theirs = parent/mainline | `merge3` doc comment; management prompt "child agents merge into their parent" | live |
| diffy, Diff3 style, marker length 7, label substitution | `lib/compute/src/merge.rs` `merge3` | built and tested; **no caller wired** at this commit |
| Trailing-newline rule | same file, `merged_has_trailing_newline` | same |
| Marker variants `(modified)` / `(deleted)` | `local/src/lib/worktree/conflict/markers.ts` | live (detection only) |
| Resolution = no markers left in any conflicted path, then merge again | `conflict/resolver.ts` `checkConflictResolution`; prompt "resolve conflict markers and call merge again" | live |
| Open conflict carries base/ours/theirs/paths | `FsConflictOpenedPayloadSchema` in `lib/events/src/repo.ts` | live |
| Blocked outcomes (missing object, internal) leave the child untouched | routing `merge.ts` `appendParentMergeTerminal(kind: "blocked")` | live |

Deliberate divergences:

- **Mode-only changes** are ignored by the plugin's overlap check and
  merged by "side that changed it". Graphcoder compares whole entries
  and would call a mode-vs-content change a conflict.
- **Text gate** (UTF-8, NUL, size) is a plugin addition. Graphcoder has
  no byte-level gate because `merge3` only ever receives strings.
- **Where markers are written.** Graphcoder's live tree-level merge does
  not write markers anywhere yet; the marker path is a contract
  (detection, resolution, event payload, prompt) waiting for `merge3` to
  be wired. This plan implements that contract against the fork
  workspace, which is the only place the plugin can write without
  touching the mainline.

## As built (deviations from the plan above)

- **Base label.** diffy labels the middle block `||||||| original`, and
  graphcoder's `merge3` only rewrites the ours/theirs labels, so its real
  output says `original`, not `base` as its doc comment claims. The port
  matches the code; the modify/delete block is built by hand with the
  same label.
- **Landing.** When nothing merged by content and nothing conflicted,
  every landing path is a fork-only subtree that the fork's snapshot F
  already holds exactly, so F is the landing source and no M is built
  (the timeline row reads `fork <id> snapshot (N paths)`). Otherwise
  M = H + entries is built as an unpublished generation and is the
  source (`fork <id> merge (a merged, b replayed)`). Either way the
  landing paths are written by ONE `restore_paths` call: no safety row
  (publish_head captured the tree moments before), one write pass, and
  the written paths captured directly when they are all regular files
  or symlinks (a directory or an absent path falls back to a watcher
  drain), recorded as the single landed row. The `before promote …`
  row records the published head without another drain. R is built
  from M with the marker files on top. A rebase writes R − F into the
  fork through its mounted path so driver and kernel caches remain coherent.
- **Rebased forks land through snapshot/apply.** A rebased fork's checkout
  still sits on the head it was cut from, so an optimistic commit against the
  new head would conflict.
- **Subtree copy and removal are iterative.** The fs facade's futures
  are large enough that three nested levels overflowed the pipeline
  thread's 2 MiB stack in a debug build. Both walks use an explicit work
  list, and the pipeline thread reserves 32 MiB.
- **`rewind --last` is not the undo.** It targets the latest real
  checkpoint, which after a merge is the landed row. The undo is the
  `before promote <id> (merge)` safety row, exactly as for a replay.
- **Wire.** `PromoteInfo` gained `merged_files`, `conflicts`, and
  `fork_path`; `ForkEntry` gained `conflict_paths` and `conflict`
  (base/ours/theirs). A conflicting promote returns a non-zero exit
  with the per-file report on stderr.
- **Gitignored paths never conflict (found by a live `/fork` round,
  2026-09-10).** Each fork's test run regenerated `__pycache__/*.pyc`;
  three forks then all held a different binary at one path and every
  later promote refused. The daemon now asks `git check-ignore
  --no-index` about the contested paths only; ignored ones are dropped
  from refusals and conflicts, the mainline keeps its own copy (a rebase
  writes it into the fork), and promote reports `kept the mainline's
  copy of N gitignored path(s)`. Without git, or outside a repo, nothing
  changes. `merge.sh` G27 covers both the landing and the conflict path.
- **Adjacent lines are one hunk.** The README's three bullets sit on
  consecutive lines, so diff3 saw one region and the round conflicted
  twice on it (resolved correctly by the agent both times). The skill
  now says adjacent lines are one region and to give such blocks to one
  fork. Inherent to three-way line merge; git behaves the same.
- **Live e2e:** `tests/acceptance/claude-merge-e2e.sh` runs a real
  `/fork` PARTITION round over one shared file and a deterministic
  conflict the agent must resolve and promote; opt in with
  `ACYCLIC_E2E=1` in `run-all.sh`.
- **No directory swap on promote (rough-edge pass, 2026-09-10).** An
  unmoved mainline used to land by whole-tree swap, which replaced the
  repo directory and needed the skill's `cd "$PWD"` step. Promote now
  always goes through the merge path: with an unmoved head the plan is
  "take every fork path" and they are written in place. `merge.sh` G11
  asserts the repo inode survives a promote.
- **Diff output marks gitignored paths** with a `(gitignored)` suffix
  and a `(K gitignored)` count so the skill's smallest-diff rule can
  ignore cache and build noise; the paths stay listed because rewind
  restores them. `acyclic diff` also accepts generation hex prefixes,
  which is what `promote` prints and what an agent reached for.
- **Rebases into mounted forks are written through the mount, not the
  checkout (CI on FUSE, 2026-09-10).** Writing R − F into the shared
  checkout behind the mount's back left the driver's and kernel's name
  caches stale: a file the fork had deleted and the rebase recreated
  stayed invisible for good on the macOS NFS driver even after
  `invalidate` returned Ok, and the FUSE transport (Linux, FUSE-T on the
  macOS runner) has no invalidation at all. The daemon now writes the
  rebase into `<mount root>/<id>/…` with plain filesystem operations
  (`merge::materialize_paths`), so every cache saw the operation. Promote
  also no longer detaches the
  route before working; it snapshots under the checkout lock and drops
  the route only after the fork lands, which removed the negative-entry
  window that hid re-attached forks on Linux.
- **Latency gate was measuring the wrong branch (2026-09-10).** The
  macOS CI gate went from 8 ms to 115–294 ms at ce39008, which made plain
  `acyclic checkpoint` wait for the capture by default. The gate ran plain
  `checkpoint`, so it timed the drain loop's 50 ms quiesce window plus a
  20k-file snapshot instead of the hook's enqueue-ack. Bisected with
  release builds of four commits, then proven with `ACYCLIC_TRACE=1`: the
  wait path costs ~77 ms even on 300 files, the enqueue path and the real
  post-tool hook ack in 0.1–0.3 ms. The gate now samples `--no-wait` and
  prints the wait-path p95 as information.
- **Path tracing.** `ACYCLIC_TRACE=1` makes the CLI, hook, daemon, and
  pipeline log every branch taken and its cost (client connect/spawn,
  op dispatch and reply, WAIT vs ENQUEUE checkpoint,
  recovery baseline, watcher drain polls/batches/stop reason, snapshot,
  index, publish, promote mode and outcome, merge plan counts, root
  hints). Daemon lines land in the store's `daemon.log`.
- **Root-targeted watcher hints.** Root-targeted hints are now
  stripped before capture: metadata-only ones dropped, structural ones
  (root created/removed/renamed, i.e. a mount came or went) trigger a
  fresh watcher and a recovery baseline instead of a failed request.

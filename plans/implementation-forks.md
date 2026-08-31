# Launch 3: Forks — implementation plan

Ships `03-forks.md`: N-way local forks, pick the winner. Built on fs's native
mounts — the resolution of the plan's open CoW question is **mounts, not
reflinks**: fs now ships qualified FUSE-T/FUSE/ProjFS drivers and the mount
path is the strategic asset (lazy hydration, Launch 4's filesystem-level
enforcement). Reflinks remain a fallback if mount UX disappoints.

## How a fork works (the fs wiring, verified against fsd's usage)

```
pipeline: commit()                        → fork base = published head H1
volume.checkout(Head, ReadWrite/PrivateOverlay)  → one checkout per fork
SharedCheckout::new(checkout)             → mount-safe serialization boundary
CheckoutMountSource::new(shared, config)  → callback runtime
mount_native(NativeMountRequest{writable: true, destination}, source)
                                          → NativeMountSession (Drop unmounts)
```

Each fork is a **writable overlay mount**: reads hydrate lazily from the
store (true O(1) creation — node_modules included, no copying), writes
accumulate in that checkout's private overlay, invisible to the real tree
and to other forks. Fork dirs live at `<repo-parent>/.<repo>.forks/<id>/`.

## Promote (v1: unmoved mainline only, per the acceptance criteria)

1. Pipeline publishes its pending state. If the tree moved since the fork's
   base, the fork's later commit conflicts → **legible conflict report**,
   never silent corruption. (Three-way Merkle merge is the v2 of this
   launch.)
2. Fork checkout commits (Optimistic, expected head = H1) → head H2.
3. Pipeline re-checkouts Head, then reuses the **rewind swap machinery**
   (materialize H2 → journaled atomic swap → re-baseline) to land the
   winning tree in the real directory. Index row: `manual`,
   label `promote <fork>`.
4. Losing forks: unmount, drop checkout (overlay evaporates), delete dir.

## Phases

**Phase F0 — mount qualification spike** (`acyclic-qual mount-smoke`):
prove on this machine that a writable FUSE-T mount of a real checkout
serves reads correctly (content vs materialize), accepts writes into the
overlay, survives unmount, and measure first-read hydration latency.
Gate: verdict appended to phase0-verdict.md; if FUSE-T UX fails, pivot to
reflink clones before any product code.

**Phase F1 — engine (`fork.rs` + pipeline ops):** ForkManager (create N,
list, drop, promote), fork base commit, conflict-legible promote via the
rewind swap, index attribution. Daemon owns `NativeMountSession`s (mounts
need the long-lived process). Crash story: sessions die with the daemon —
startup sweeps `.forks/` for stale mount points and cleans them.

**Phase F2 — surfaces:** `acyclic fork [-n N]`, `acyclic forks`,
`acyclic fork-drop <id>`, `acyclic promote <id>`; proto ops; timeline rows.
Skill teaches fork-and-try to the agent.

**Phase F3 — acceptance (`tests/acceptance/forks.sh`):** fork 2 →
divergent writes through both mounts → main tree untouched → promote one →
real tree = winner's content (byte-verified), loser evaporates → promote
after mainline moved → legible conflict. Fork creation timed (<1s target
on the node_modules fixture).

## Status & punch list (2026-08-31)

Done: F0 gate PASSED (mount-smoke: attach 86ms, hydrate 8MB in 38ms, overlay
isolated, clean detach). F1/F2 code built: `fork.rs`, pipeline Fork/Promote,
daemon fork map + mount lifecycle (block_in_place), proto ops, CLI verbs.
Single fork verified live (FUSE-T NFS mount attached, listed, in mount table).

Found along the way: mount drivers block_on inside their own runtimes (daemon
must wrap lifecycle calls in `block_in_place`); short fork ids must come from
UUIDv7's random tail, not its timestamp head; **fs bridge enforced one FUSE
session per process** (`active_fuse` global, exit code 3) — patched to a
context-keyed registry with targeted interrupt (fs suites 573+21 green);
further mount issues now being fixed upstream by hand.

Remaining, in order:
1. [fs, in hand] finish the upstream mount fixes; fs suites green.
2. Re-run `mount-smoke`, then `fork -n 3`: three simultaneous mounts attach,
   `acyclic forks` lists them, mount table shows 3.
3. Divergence check: different writes through each mount; forks isolated
   from each other and from the untouched mainline.
4. Promote journey: `promote <A>` lands A's tree byte-verified in the repo
   (gitignored state included), B/C evaporate, timeline row recorded;
   `fork-drop` and daemon-stop cleanup leave no mounts or dirs behind.
5. Conflict path: move mainline after forking (`checkpoint --durable`),
   promote → legible conflict, tree untouched.
6. Idle-fork interaction: confirm the pipeline's idle/durable commits don't
   move head under live forks unexpectedly (they do move it — decide
   whether fork-base staleness from OUR OWN noop commits should count as
   "moved"; likely compare generations, not heads).
7. Encode 2–5 as `tests/acceptance/forks.sh`; wire into run-all; creation
   time asserted (<1s). Unit tests: forks_root/sweep (done), promote
   conflict classification.
8. Full suite 2× + Linux docker run (FUSE needs `--device /dev/fuse
   --cap-add SYS_ADMIN`; add with graceful skip) — Linux fuse path uses
   crates.io fuser, untouched by the FUSE-T bridge work.
9. Docs: fork verbs in README/skill; verdict + memory updates; commit
   plugin repo; fs changes reviewed/committed by owner.

## Second-mount stall: audit verdict (2026-08-31)

Two independent audits + live experiments: **not a plugin bug.** Every
plugin layer cleared (two LocalFs per store share a *shared* flock; SQLite
busy-timeout bounds at 5s; block_in_place wiring is stricter than fsd's own;
mount callbacks never reach our code — the second session dies during
FUSE_INIT). Primary evidence (`~/Library/Logs/fuse-t/fuse-t.log`): the
second `go-nfsv4` helper prints `Failed to listen: 52100` and **never
advances to 52101**, while helpers contending with pre-existing established
mounts walk the whole pool — a port-walk race in the vendor helper when two
sessions start from one process in a burst. A disassembly-level suspect also
exists one layer down (libfuse-t serializes all transport reads behind one
process-global mutex held in blocking recv), but the poke-mount-0 unstick
test did not confirm it as the operative mechanism here.

Latent fs issues surfaced for upstream: (1) libfuse-t's `_cpid`/
`_monitor_fd`/`_mount_wait_thread` are single globals — second mount
clobbers them, making FIRST-session teardown unsound with two mounts;
(2) `is_mounted` is an unbounded `stat` that can hang forever against a
dead hard NFS mount, defeating the 10s deadline; (3) `transport_capacity_
check` counts visible mounts, not port-holding helpers, so its "unlikely to
be pool exhaustion" message is confidently wrong in exactly the failing
case; (4) fs has **zero** tests mounting two sessions in one process — the
registry fix was validated only to "second session starts", not "becomes
visible".

Decisive next fs experiment: pre-bind 52100 with an unrelated listener and
run a SINGLE mount — if the helper walks to 52101, the walk only breaks
against same-burst siblings (helper race); if it stalls, the pool is
effectively one-port-per-burst and the concurrency story reframes.
Fallbacks if vendor-limited: serialize+stagger session starts in
`FuseTSession::start`, or helper-process-per-fork on macOS (Linux FUSE
unaffected).

## Known risks

- Writable FUSE-T semantics under agent workloads (editors, git, build
  tools inside the mount) — exactly what mount-smoke and forks.sh probe.
- Multiple simultaneous `CheckoutMountSource`s each spawn a callback
  runtime; N is small (forks are handfuls, not hundreds).
- Subagent orchestration (launching N host sessions rooted in N forks) is
  deliberately NOT in this milestone — the substrate ships first; the
  orchestration story follows once fork UX is proven.

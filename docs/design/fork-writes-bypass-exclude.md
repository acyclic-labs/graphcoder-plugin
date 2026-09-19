# Finding: `exclude` does not govern what a fork writes

**Status: observed, reproducible, not fixed.** Recorded 2026-09-18 from a live
run against this repo. The numbers below are from the object store's own
timestamps, not from estimates.

## What happened

A speculative agent worked inside a fork mount of this repo (`acyclic fork -n 1`)
with `exclude = ["target"]` in `.acyclic/config.toml`. The agent had no shell
and no LSP tool. At T+32s a `target/` directory appeared in the fork anyway —
the host's edit-time diagnostics ran `cargo check` on the agent's behalf. No
tool permission controls that; it is the host's own machinery.

The object store then grew like this, on a repo whose tracked source is 2.4 MB:

```
T+1–5 min   1,267 MB   106,040 objects   while the agent worked in the fork
T+8 min     2,259 MB    19,924 objects   `acyclic promote` snapshotting the fork
T+9–10 min    566 MB    40,474 objects   the rewind, then a recovery rescan
total       4.6 GB     171,485 objects
```

At the promote the backend answered `Objects capacity exhausted`. The
rewind's exclusion cleanup hit the same wall, the daemon fell into
`baselining`, and every subsequent operation — `restore`, `checkpoint`,
`status` — failed with the same error. The store was unusable ten minutes
after `init`.

## Why

`exclude` is applied when the main tree is captured: excluded paths never
enter a checkpoint, and [05-monorepo.md](05-monorepo.md) notes that forks and
Safe Mode sessions do not *see* excluded paths, because the base generation
does not hold them. That is consistent with what was observed — the fork
started without a `target/`.

What is not covered is a path that is *created inside the fork*. A fork is a
writable overlay backed by the object store, so every write lands in the
store as it happens. Nothing consults the exclude set on that path. Build
output written into a fork is therefore captured in full, and `promote`
snapshots the fork — `target/` included — before landing it. The first promote
in this run reported exactly that seam from the other side:

```
merge stopped while landing 3 path(s): restore: target is excluded from
snapshots (`exclude` in .acyclic/config.toml); no checkpoint holds it.
```

It had captured `target` as a fork path, then refused to restore it as an
excluded one.

## Why it matters more than it looks

- On a Rust tree, one `cargo check` is enough. The fork's `target/` was 1,391
  paths; three concurrent forks in an earlier run reached 3.8 GB the same way.
- The write is not the agent's choice. Denying Bash and LSP did not prevent
  it, because the host's diagnostics are not a tool the agent invokes.
- The failure is not a slow store, it is a wedged one: capacity exhaustion
  stops `restore` and the rewind path, which are the recovery tools.
- The design's own guidance — "speculation may use idle capacity and must
  never compete for busy capacity" — cannot hold if a fork's build output is
  hashed at full rate while the fork is in use.

## What would fix it

In rough order of how much they change:

1. **Apply the exclude set to fork writes.** A path that would be excluded
   from the main tree's capture should be excluded from a fork's overlay too:
   written through to a scratch location, never stored, never snapshotted.
   This is the fix that matches what `exclude` already means.
2. **Have `promote` skip excluded paths** rather than stop on them. Necessary
   even with (1), for forks created before the rule landed.
3. **Refuse the snapshot before exhausting the store.** A promote that is
   about to capture an excluded directory of a thousand paths should say so
   and stop, not discover capacity limits halfway through.

Until one of these lands, the operational workaround is to remove excluded
directories from a fork before promoting it, and to keep build output out of
fork mounts entirely — which no permission setting can guarantee.

## Related

- [03-forks.md](03-forks.md) — fork design; this is a gap in what the overlay
  captures, not in how it isolates.
- [05-monorepo.md](05-monorepo.md) — exclude semantics for the main tree.
- README, *Configuration* — `exclude` is path-based; this finding is the other
  half of that: it is also main-tree-only.

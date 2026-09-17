# Launch 3 — Forks

The fork engine. The headline demo launch.

## Features

7. **N-way local forks** — fork the working tree in O(1), run N subagents on N forks in parallel, present diffs side by side; the dev picks the winner, losers evaporate.
8. **Speculative execution** — the agent forks before a risky step and continues on the fork; success promotes the fork, failure never touches the real tree.
9. **Test-in-fork** — run tests or builds against a frozen fork while the agent keeps editing the main tree.
10. **What-if forks for the dev** — human-facing: "fork my tree, try the framework upgrade over there, show me the damage report" — experiments without stashing anything.

## User journey

1. A flaky race condition has resisted two fix attempts. Maya prompts: "fix this — try three genuinely different approaches in parallel."
2. The plugin forks the working tree three ways in under a second — node_modules included, no copying — and launches three subagents, each rooted in its own fork.
3. While they work, she keeps using the main session on an unrelated task. The forks can't step on her tree or each other.
4. Ten minutes later: a comparison table — approach A: 4 files, tests green; B: 1 file, tests green, but changes a public API; C: tests still flaky. Diffs side by side.
5. She picks A: `acyclic promote` merges it onto her (since-moved) tree via three-way Merkle merge; B and C evaporate.
6. Separately, before a scary dependency upgrade, she runs the what-if flow: the agent tries the upgrade on a fork and reports the damage before her real tree is ever touched.

## What was built

This section previously read as a forward plan. Launch 3 shipped; what follows
is what exists, including where it diverged from the plan.

- **Copy-on-write materialization** — decided as **mounts**, the opposite of the
  lean this doc recorded. There is no reflink or `clonefile` path in the
  codebase. All forks are routes inside **one** kernel mount, not N mounts.
  Without a mount provider every fork is a **full copy**, so the O(1) claim holds
  in mount mode only — the degraded path is keyed on mount availability, not on
  filesystem reflink support.
- **Fork workspace management** — partially. Teardown and placement exist;
  per-fork port allocation and env provisioning were dropped as out of scope.
- **Subagent orchestration wiring** — shipped, but as a prompt-level skill and a
  `/fork` slash command where the root agent hands each child an absolute fork
  path. There is no working-directory injection at the hook layer.
- **Merge and promote machinery** — shipped, and not as described. Promote does
  **not** do a root swap; it writes the fork's paths in place, one at a time.
  Content merges are line-level diff3, not Merkle-only. On conflict, nothing
  lands: the fork is rebased onto head and markers are written *into the fork*
  for the user to resolve and promote again.
- **Comparison surfaces** — `fork-diff` gives the blast radius of a live fork
  against its base.

### Constraints that were never written down

- **Forks do not survive a daemon restart**, which sits badly with the
  "test-in-fork" framing above.
- **Fork count is hard-capped at 16**, and the decompose policy defaults to
  `fan_out=3`, `max_depth=2`, `max_forks=8`.
- **Merge refuses** files over 4 MiB, binaries, kind changes, and directory
  ancestry conflicts. Gitignored paths changed on both sides are silently never
  merged — the mainline keeps its copy.
- **Adjacent lines count as the same region**: a three-way merge cannot split a
  hunk, which is why the partitioning advice in the skill is a product
  constraint rather than a style preference.

## Acceptance criteria

- Fork creation < 1s on a tree with node_modules. **Not measured as written**: the
  gate asserts 3 forks in under 5s on a small fixture repo with no
  `node_modules`, and skips entirely when mounts are unavailable.
- 3 parallel subagents on 3 forks complete without cross-contamination.
  **Verified as isolation via direct writes**, not with three live subagents.
- Promote onto an unmoved tree is atomic; promote onto a moved tree produces a
  correct three-way merge or a legible conflict report — never silent
  corruption. *Met.*
- ~~Main session remains fully usable while forks run.~~ **Contradicted by what
  shipped.** The installed skill instructs the operator to make no edits to the
  real tree while forks are live. The engine guarantees only that checkpoint,
  diff and rewind keep working — not that concurrent editing is safe.

## Notes

- Fork orchestration of subagents is uniquely plugin-shaped — it must live inside the host.
- Filesystem-layer enforcement for Launch 4's guarded paths arrives with the mount option. This turned out to be the decisive argument: the mount shipped and reflinks never did, and Safe Mode refuses to start without a mount provider.

## Open questions (not yet settled)

1. **Mount-provider reliability is unowned.** The macOS path depends on a vendored NFS server with a shared port pool; orphaned helper processes can wedge future mounts, and the acceptance suite reaps them before every run. Nothing treats this as a supported-configuration question.
2. **Copy-mode forks are unguarded.** The guard wraps mount routes only, so on a host without mounts, guarded paths are unenforced even for explicit forks.
3. **Partial failure of `fork -n N`** leaves forks 1..K-1 live and reports the error. No policy says whether that is correct.
4. **Should the 16-fork cap, and the decompose defaults, be policy rather than constants?** They are currently hardcoded in the server.
5. **Does the freeze rule stay?** Requiring the operator to stop editing while forks are live is a real product constraint that the launch's own journey contradicts.

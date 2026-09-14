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

## What has to be built

- **Copy-on-write materialization** — the launch's hard decision. A fork is free in the Merkle DAG (copy a root hash); the work is giving each fork a real directory processes can run in. Options:
  - *Local FUSE/NFS mount* serving trees lazily from the store — true O(1), lazy hydration, but a filesystem driver to maintain and macOS FUSE pain.
  - *Per-file CoW clones* via APFS `clonefile` / Linux reflinks — no driver, near-instant on supported filesystems.
  - *Overlay directories* — portable, slowest.
  - **Current lean: reflinks/clonefile first, mount later.**
- **Fork workspace management** — where fork directories live; reproducing the environment a build needs (node_modules and virtualenvs via CoW clone, port allocation per fork, env vars); cheap teardown.
- **Subagent orchestration wiring** — launching N host-tool subagents each rooted in its own fork (working-directory injection via the plugin/hook layer), tracking lifecycle, collecting per-fork results.
- **Merge and promote machinery** — promoting a winning fork is a root swap plus restore; merging a fork onto a tree that moved needs Merkle-diff three-way merge with conflict surfacing. This is the seed of the agent-native VCS and dVFS's realtime merge.
- **Comparison surfaces** — side-by-side multi-fork diff (files changed, tests passed, summary per attempt) so picking a winner takes seconds.

## Acceptance criteria

- Fork creation < 1s on a tree with node_modules (measured on APFS and ext4/btrfs; degraded path documented for filesystems without reflink).
- 3 parallel subagents on 3 forks complete without cross-contamination (verified by conflicting writes to the same paths).
- Promote onto an unmoved tree is atomic; promote onto a moved tree produces a correct three-way merge or a legible conflict report — never silent corruption.
- Main session remains fully usable while forks run.

## Notes

- Fork orchestration of subagents is uniquely plugin-shaped — it must live inside the host.
- Filesystem-layer enforcement for Launch 4's guarded paths arrives with the mount option; that dependency is an argument for eventually building the mount even if reflinks ship first.

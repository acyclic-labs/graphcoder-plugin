# Launch 4 — Safe Mode

Fork-engine features for trust-sensitive teams. Depends on Launch 3.

## Features

11. **Dry-run mode** — the whole session runs against a fork by default; nothing hits the real tree until the dev approves the final diff.
12. **Ephemeral scratch trees** — disposable full-repo copies for destructive exploration that vanish on session end.
13. **Guarded paths** — files and directories the agent's writes can't touch, enforced at the filesystem layer rather than by prompt hope.

## User journey

1. Maya's team lead wants agents on the codebase but not agents' mistakes in it. They check in `.acyclic/config.toml`: dry-run on by default, `.env` and `migrations/` guarded. Everyone who clones the repo inherits the policy.
2. A new teammate starts a session. Unknown to them, the whole session is rooted in a fork — paths look normal, tools behave normally, the real tree is untouched.
3. The agent needs to test a destructive codemod, so it grabs a scratch tree, runs it, inspects the wreckage, and lets it vanish.
4. Mid-task the agent tries to edit `.env`. The write is refused at the interposition layer — not because the prompt said please don't.
5. The session ends with a final diff: "14 files, here's everything." The teammate reviews, approves, and only then does the real tree change — atomically.

## What has to be built

- **Session redirection** — transparently rooting the entire host-tool session in a fork (working-directory redirection at session start via the plugin layer), while keeping paths the dev sees looking normal.
- **Approval-gated apply** — final-diff review flow: Merkle-diff the fork against the real tree, present it, apply atomically on approval, discard on rejection.
- **Write interposition for guarded paths** — the enforcement point decides the strength: hook-level blocking (intercept Edit/Write/Bash targets) works without a driver but can be bypassed by arbitrary shell; real filesystem-layer enforcement needs the Launch 3 mount (read-only/deny rules served by the driver). Ship hook-level first, upgrade with the mount.
- **Policy configuration** — per-repo declarative config (checked in, so teams share it) for guarded paths, dry-run defaults, scratch-tree limits.
- **Scratch-tree lifecycle** — auto-created CoW workspaces tied to session lifetime; guaranteed destruction on exit or crash (janitor process).

## Acceptance criteria

- A dry-run session is indistinguishable from a normal one to the agent (tool behavior, paths, git status) — measured by an agent completing a standard task unaware.
- Guarded-path writes are refused with a legible error the agent can act on; with the mount, refusal holds against arbitrary shell commands.
- Rejected sessions leave zero trace on the real tree; approved applies are atomic.
- Orphaned scratch trees from a crashed session are collected within one janitor cycle.

## Open design risks

- Session redirection fights host assumptions (path display, git status confusion) — validate against real host behavior before promising transparency. Known unverified assumption; see overview.
- Hook-level guarded paths may not be worth shipping alone (Claude Code permission deny rules already cover the weak version); the differentiated form is filesystem-level and mount-dependent.

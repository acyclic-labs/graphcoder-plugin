# Launch 4 — Safe Mode

Fork-engine features for trust-sensitive teams. Depends on Launch 3.

Status: **built, mount-dependent.** Claims below are tagged *shipped*,
*not built*, or *diverged* where what exists differs from what was planned.

## Features

11. **Dry-run mode** — the whole session runs against a fork; nothing hits the
    real tree until the dev approves the final diff. *Shipped, off by default.*
12. **Ephemeral scratch trees** — disposable full-repo copies that vanish on
    session end. *Diverged: the auto-drop machinery exists and runs at session
    end, but nothing creates a tagged scratch tree. There is no user-facing
    scratch verb and no caller sets the session id, so this feature has a
    working destructor and no constructor.*
13. **Guarded paths** — writes the agent can't make, enforced at the filesystem
    layer rather than by prompt hope. *Shipped for mounted forks and Safe Mode
    sessions only. See the first open question — outside those, it is a no-op.*

## User journey

The journey as originally written is still the target. Two steps do not work
as described today:

1. Maya's team checks in `.acyclic/config.toml`: dry-run on, `.env` and
   `migrations/` guarded. Everyone who clones inherits the policy. *Shipped.*
2. A teammate starts a session rooted in a fork; paths look normal, the real
   tree is untouched. *Shipped, if the host has a mount provider.*
3. The agent grabs a scratch tree for a destructive codemod and lets it vanish.
   *Not built — see feature 12.*
4. The agent tries to edit `.env`; the write is refused at the interposition
   layer. *Shipped inside the session. Note the macOS caveat below: the refused
   write can still report success to the shell.*
5. The session ends with a final diff the teammate reviews and approves.
   *Diverged: `session-resolve`, `session-apply` and `session-discard` are
   hidden CLI-only verbs. No host adapter wires them, they are not MCP tools,
   and session end does not resolve the session — the shadow mount stays up.*

## What was built

- **Session redirection** — *shipped.* The session is rooted in a shadow mount.
- **Approval-gated apply** — *shipped as CLI verbs, unwired.* See journey 5.
- **Write interposition for guarded paths** — *diverged, and in the opposite
  order to the plan.* The plan was to ship hook-level blocking first and
  upgrade with the mount. Hook-level was never built: the hook contract is to
  never block and always exit 0, and it never reads `guarded_paths`. Only the
  mount-level guard exists, so enforcement and Safe Mode arrived together.
- **Policy configuration** — *shipped, minus scratch-tree limits*, which have
  no config key because there are no scratch trees.
- **Scratch-tree lifecycle** — *diverged.* There is no janitor process.
  Session-owned forks are dropped best-effort in the session-end handler, and a
  crashed daemon's shadow mount is reaped opportunistically on the next CLI
  invocation — an on-demand sweep, not a cycle.

### Constraints that were never written down

- **One Safe Mode session per repo.** A second is refused outright.
- **Apply requires an unmoved mainline.** If the real tree moved during the
  session, the choice is rewind to base or start again — the doc promised an
  atomic apply with no such condition.
- **Mainline capture is suspended for the whole session.** A dry-run session
  records no mainline checkpoints.
- **Guarded paths are excluded from the session diff entirely**, so the
  approval view never shows them.
- **The guard surface is wider than "writes"**: it covers rename and hard-link
  on both source and destination, and unconditionally refuses AppleDouble
  sidecars.
- **Reflink acceleration is disabled whenever any guard is configured**, so
  guarded repos silently fall back to read-and-write copies.

## Acceptance criteria

- A dry-run session is indistinguishable from a normal one to the agent.
  **Unverified** — there is no live-host Safe Mode test; the suite drives the
  CLI with a synthetic host name.
- Guarded-path writes are refused with a legible error the agent can act on.
  **Partially met, with a known correctness caveat**: on macOS a refused write
  can still return exit 0 to the shell because of NFS write-back caching, so
  the agent may see success where the filesystem saw refusal.
- Rejected sessions leave zero trace on the real tree. **Met** — and the
  converse is now explicit: a crash discards all session work. Zero trace and
  zero recovery are the same property.
- Orphaned scratch trees collected within one janitor cycle. **Not met** — no
  janitor exists.

## Open questions (not yet settled)

1. **Guarded paths are a no-op in the default configuration.** With
   `dry_run = false` (the default) and no fork, `guarded_paths` is never
   consulted and the agent can write `.env` freely. A team that checks in
   guarded paths and reads the README table has every reason to believe
   otherwise. This is the highest-severity gap in this launch. *No lean — it
   is either a documentation fix or a feature.*
2. **Silent degradation on a host without mounts.** Safe Mode refuses to start,
   the session-start op fails, and the hook prints to stderr and exits 0. A
   repo with `dry_run = true` checked in therefore runs against the real tree
   with no visible failure. *Current lean: this should be loud — a refusal to
   proceed rather than a warning nobody reads.*
3. **Copy-mode forks are unguarded**, because the guard wraps mount routes
   only. On a mount-less host guarded paths are unenforced even for explicit
   forks. *No lean.*
4. **Do the approval verbs get wired into hosts?** They are the visible half of
   the feature and are currently reachable only by someone who reads the hidden
   CLI surface. *Current lean: yes, and Safe Mode should not be promoted until
   they are.*
5. **Is the NFS write-back caveat acceptable?** It weakens the acceptance
   criterion it contradicts, and the failure direction is the dangerous one —
   the agent believes a guarded write succeeded. *No lean.*
6. **Should scratch trees be built or dropped from the feature list?** The
   destructor exists; the constructor does not. *Current lean: drop the claim
   until there is a verb.*
7. **Hardcoded constants that are really policy**: the 16-fork cap, the 5s reap
   probe, and the 2s pre-tool wait after which the tool proceeds uncheckpointed.

## Open design risks

- Session redirection fights host assumptions (path display, git status
  confusion). Still unvalidated against a real host.
- Hook-level guarded paths may not be worth shipping alone. **This was
  settled by not doing it** — the mount-level guard shipped instead, which
  means guarded paths inherit every one of Safe Mode's platform constraints.

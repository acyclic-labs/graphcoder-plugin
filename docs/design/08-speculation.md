# Speculation — computing before anything asks

Cross-cutting, not a launch: it makes existing verbs faster and adds one.
Launch 3 named "speculative execution" as forking before a risky step
([03-forks.md](03-forks.md), feature 8); this is the other half — using the
time when nobody is waiting.

## The idea

The daemon knows things a request does not. It sees the prompt before the
agent acts, it knows when a turn closed, and it knows when the watcher went
quiet. Those are all moments when the machine is idle and the answer to a
question nobody has asked yet is already determined.

Two things are computed ahead:

- **The previous-session brief**, when a session ends. This is the most
  expensive thing on the agent's critical path — `SessionStart` blocks on it
  and prints it into the model's context — and it costs a pipeline diff per
  abandoned branch plus two more. The session that will read it ends long
  before it is asked for.
- **A turn summary**, when the next turn starts. This is the only kind that
  runs a model, and the only thing in the product that spends money.

## Correctness comes from the key, not from invalidation

A speculative result is identified by the generation(s) it describes.
`GenerationId` is a Merkle id, so equal id means a bit-identical tree: a
result computed against a tree that has since moved simply never matches the
key a later request builds. Serving a near-miss is structurally impossible
rather than guarded against.

Everything follows from that:

- There is no staleness check anywhere, and no "is this still valid?" path to
  get wrong.
- Every cancellation in the daemon is a **spend** control, never a
  correctness one.
- Eviction is about disk, not truth: a stale entry is already unreachable.

Keys carry more than generations where the answer depends on more. A brief is
keyed on the subject session plus the head generation, because another
session recording a checkpoint changes which session the brief is about. A
summary carries the model and prompt template in its `recipe`, so changing
the model stops serving the previous model's prose instead of mixing the two.

## Where things run

| | Runs on | Why |
|---|---|---|
| Brief precompute | scheduler thread → `PipelineHandle` | Same cost as a real diff; queues behind real work |
| Model run | spawned task on the scheduler runtime | 45 seconds of IO; must not block the event loop |
| Claim | the requesting connection | Two indexed queries and one `UPDATE` |

**Nothing speculative runs on the pipeline thread.** That thread serves the
hook path. The scheduler reaches it only through
`PipelineHandle::diff_speculative`, which abandons the request rather than
queue it when the channel is loaded: speculation may use idle capacity and
must never compete for busy capacity.

**Nothing speculative is load-bearing.** A full queue, a wedged cache, a
missing database, a body that no longer deserializes — every one falls
through to computing the answer the way it was computed before. The claim
path opens `spec.db` with a 200ms busy timeout rather than the usual five
seconds, because stalling a session start behind a lock is worse than not
speculating.

## `spec.db`, not `index.db`

The cache is its own database. `index.db`'s write connection is owned by the
pipeline thread and used synchronously, so a second writer contending for
SQLite's single write lock would block the thread every hook call waits on.
Contention would realistically be microseconds, but the downside of being
wrong is "every hook stalls" and the upside is being able to `JOIN` — a bad
trade.

It also starts with `PRAGMA user_version = 1` rather than inheriting
`index.db`'s introspection-driven migrations, and it is pure cache: missing,
corrupt or deleted all degrade to speculation off.

`spec_cache`'s unique index on the key does double duty. It makes the key a
real key, and `INSERT … ON CONFLICT DO NOTHING` is simultaneously the dedupe
and the admission gate, so two triggers cannot both spend on one answer.
`spec_log` exists separately because the number that matters — a **miss** —
leaves no row in the cache.

## The model run

The one place the daemon spends money and the one place repo content leaves
the machine, so the constraints are about those, not about speed.

- **The child gets no route into the repository.** Its working directory is
  an empty scratch dir; its entire input is stdin, built from a
  store-computed diff that already honours `exclude` — paths and the prompt
  excerpt, never file contents. A summarizer does not need a tree to walk,
  and handing an agent CLI the real working tree would invite it to write
  files, which would race the watcher and manufacture checkpoints nobody
  asked for.
- **It runs in its own process group.** An agent CLI is a runtime that spawns
  children; killing only the process we spawned would leave them running and
  billing. A timeout `SIGTERM`s the group and `SIGKILL`s it after a grace.
- **Pid files, swept before the store opens**, beside the fork sweep and for
  the same reason: a crashed daemon's child is still running. The recorded
  command name is checked against the live process so a reused pid is never
  signalled.
- **Never on demand.** A request arriving is not consent to spend. `summary`
  reports that none exists and why, and always says where the words came
  from — this is generated prose, not a record of what happened.
- **Never a fork.** `Pipeline::fork()` requires `State::Ready`, writes a
  "fork base" checkpoint row, and publishes. A speculative fork would put
  rows nobody asked for in the timeline.

## Configuration

Its own file, `~/.config/<name>/speculate.toml`, never the checked-in repo
config. Two reasons:

1. `Config` is `deny_unknown_fields`, so a new table there would make an
   older binary fail `Config::load` — and with it every CLI verb and the
   daemon — on any machine that had opted in.
2. The repo config is checked in, and whether to spend tokens is a personal
   decision, not one a teammate inherits from a commit.

Loading never returns an error, deliberately opposite to `Config`'s contract:
a missing file, a typo, or no `HOME` all leave speculation off and the rest
of the product working. A malformed speculation config must never be able to
break `acyclic status`.

**Two gates, not one.** `enabled = true` buys the free half — precompute,
zero tokens. Spending additionally requires naming a `command` *and* adding
`"summary"` to `kinds`, so no single boolean can put anyone on the meter. The
command must be on `PATH` or absolute: a relative path would resolve against
the empty scratch dir.

```toml
enabled              = false
kinds                = ["brief", "diff"]
command              = []        # argv; add "summary" to kinds to use it
model                = ""        # informational, but part of the cache key
timeout_ms           = 45000
max_output_bytes     = 16384
max_runs_per_session = 20        # hard ceiling
min_run_interval_ms  = 30000
```

*Deferred:* a team-level `allow_agent` gate in the checked-in config is
desirable ([07-compliance.md](07-compliance.md) argues policy should ship
with the repo), but that key is exactly the one that breaks older binaries.
It ships a release after the parser does, so nobody can check in a key that
bricks a teammate's daemon.

## Measuring it

`acyclic status` prints a rollup, **only when speculation is configured** —
several acceptance scripts read that output, so the default stays what it
was:

```
speculation:   on — precompute only, no model runs
               24h: 31 run(s) · 19 of 31 claimed (61%) · median lead 8.2s · 412.0 KB out
```

Bytes and run counts, not money: the daemon cannot know anyone's pricing and
should not pretend to. The number to watch is **median lead** — how far ahead
of the request a claimed result landed. Near zero means the trigger is firing
too late to be worth anything, which is a fact about the design and not about
the idea.

## Acceptance

`tests/acceptance/speculation.sh`, in the free tier of `run-all.sh`: the
model half is driven by `stub-model.sh`, so the whole suite costs nothing and
needs no credentials.

| | Claim |
|---|---|
| S1 | Off by default: no cache, and `status` output unchanged |
| S2 | A session ending precomputes the next session's brief |
| S3 | The next session start serves it |
| **S4** | **A tree that moved is a miss, not a stale answer** |
| S5 | A miss caches its own result for the next asker |
| S6 | The scheduler stops with the daemon |
| S7 | Naming summaries without a command does not spend |
| S8 | A configured command turns model runs on, and `status` says so |
| **S9** | **A finished turn is summarised before anything asks** |
| S10 | A missing summary is reported, never produced on demand |
| S11 | A run that overruns is killed and recorded |
| S12 | No model run outlives the daemon |

S4 is the correctness proof and S9 is the feature. That a timeout reaches the
run's own children — the case that matters for a real agent CLI — is proven
in `spec_runner::tests::a_timeout_kills_the_children_too`, where the process
tree can be observed precisely.

Non-interference is `latency.sh` re-run with speculation on: 16.1ms p95
against 14.4ms off, both far under the 100ms budget.

## Not done

- **Fork-diff precompute.** The kind exists in `SpecKind` and nothing
  schedules it. Live forks go quiet between rounds of a decomposition, which
  is the obvious window.
- **Speculative race dispatch** — forking and dispatching `fan_out` children
  at prompt time, before the root agent decides to fork. A large token bet on
  a classifier that sees only the prompt excerpt the index stores, while the
  decompose skill decides with the whole conversation in view. Worth
  revisiting only with claim-rate numbers in hand.
- **A team-level gate**, as above.

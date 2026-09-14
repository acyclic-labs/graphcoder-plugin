# Launch 2 — Timeline

Same engine, adds the time dimension. Fast-follow; bundles into Launch 1 if it lands early.

## Features

5. **Turn-linked history** — every checkpoint tagged with the conversation turn that caused it; view or diff the repo as it was at any point in the conversation.
6. **Cross-session persistence** — checkpoints survive session exit; the next session can see where the last one ended and what previous attempts looked like.

## User journey

1. It's Tuesday morning. Yesterday's session ended mid-refactor with two abandoned approaches behind it. Maya types `claude`; the agent opens with: "Last session ended at checkpoint 41 — JWT refactor, approach 3, tests passing. Approaches 1 and 2 were abandoned; want a summary?"
2. She asks "what did approach 2 actually look like?" The agent diffs the repo as it was at that conversation turn against now — no reconstruction from memory, just a lookup.
3. A teammate asks why a config file changed. She runs `acyclic timeline` and finds the exact turn — and the prompt — that caused the edit.
4. One idea from abandoned approach 1 turns out to be right after all. She restores just that file from the old checkpoint into the current tree and keeps going.

## What has to be built

- **Transcript correlation** — map each checkpoint's root hash to the host tool's session and turn identifiers (from hook payloads / session transcripts), so "the repo at message 14" is a lookup, not a guess.
- **Per-repo metadata index** — a small local database (SQLite) over sessions, turns, checkpoints, and their DAG relationships; what history queries and cross-session views read.
- **Store lifecycle across sessions** — snapshot store and watcher survive host-tool exit (daemon or lazy re-index on start); GC extended to keep turn-linked roots reachable as long as their session metadata lives.
- **History surfaces** — browse/diff commands over the timeline for the dev; a compact "previous attempts" summary the agent can query at session start.

## Acceptance criteria

- Any checkpoint resolves to (session id, turn id, prompt excerpt) and vice versa.
- New session startup surfaces last session's end state and abandoned branches in one agent-readable summary < 1KB.
- Single-file restore from an arbitrary historical checkpoint without touching the rest of the tree.
- Timeline metadata survives daemon restart and host-tool crash.

## Notes

- Turn-linked history is the strongest *pure-plugin* feature (only a plugin sees the conversation) — nothing standalone can replicate it.
- This is also the compliance feature: the timeline is the change-management audit log (who/what prompt caused which change). Export format worth designing here, not later.

## Status (2026-09-06): shipped

Implemented in `acyclic-engine` (index), the daemon, the CLI, and the Claude
Code adapter; `tests/acceptance/timeline.sh` covers all four acceptance
criteria and runs in `run-all.sh`.

- **Transcript correlation** — the `UserPromptSubmit` hook (`acyclic hook
  user-prompt`) records a turn per prompt (`turns` table: session, 1-based
  turn, started_at, ≤240-byte prompt excerpt). Every later checkpoint in
  that session inherits the turn (`checkpoints.turn`). `acyclic show <id>`
  resolves a checkpoint to (session, turn, prompt); `acyclic timeline
  --session S --turn N` and `acyclic turns` go the other way.
- **Per-repo metadata index** — same SQLite database, additive migration
  (`turn`, `rewind_target` columns; `turns` table). `pre_rewind` rows now
  carry the checkpoint they rewound to, which is what makes abandoned
  branches a query rather than a guess.
- **Store lifecycle across sessions** — unchanged (daemon outlives the host;
  index is durable). No GC exists yet, so nothing needed extending.
- **History surfaces** — `acyclic turns`, `acyclic sessions`, `acyclic diff
  --turn N`, `acyclic show <id>`, `acyclic restore <id> <path...>` (single
  path, atomic swap, rest of the tree untouched, recorded as a `manual`
  checkpoint so it is itself undoable), and `acyclic brief`: the previous
  session's end checkpoint + turn + prompt, files changed, each abandoned
  branch (checkpoint range, turn, prompt, file count, rewind target) and
  drift since, rendered under 1KB. The `SessionStart` hook prints the brief
  to stdout, which Claude Code injects into the agent's context (skipped on
  `source: compact`). `/timeline` slash command installed alongside `/rewind`.

Not done / follow-ups:
- Audit-log export format (the compliance note) — the data is all in
  `index.db`; `acyclic brief --json` and the proto `Turns`/`Inspect` ops are
  the structured surface for now.
- Forks are not in the index (they live in the daemon), so dropped forks
  never show as abandoned branches; only rewinds do.
- Hosts without a prompt hook get turn-less checkpoints (everything still
  works, `turn` is just null).

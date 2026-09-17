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
  index is durable). No GC exists, so nothing needed extending — and this is
  now a settled v1 stance blocked on an upstream retention-release fact, not
  a "yet". See `07-compliance.md`.
- **History surfaces** — `acyclic turns`, `acyclic sessions`, `acyclic diff
  --turn N`, `acyclic show <id>`, `acyclic restore <id> <path...>` (**N paths**, each
  swapped atomically but not atomically *across* paths — on failure it
  reports which were restored and which were not attempted; the rest of the
  tree is untouched, and the restore is itself recorded as a `manual`
  checkpoint so it is undoable), and `acyclic brief`: the previous
  session's end checkpoint + turn + prompt, files changed, each abandoned
  branch (checkpoint range, turn, prompt, file count, rewind target) and
  drift since, rendered under 1KB. The `SessionStart` hook prints the brief
  to stdout, which Claude Code injects into the agent's context (skipped on
  `source: compact`). `/rewind`, `/timeline` and `/fork` slash commands are installed, along
  with two skills.

Not done / follow-ups:
- Audit-log export format (the compliance note) — the data is all in
  `index.db`; `acyclic brief --json` and the proto `Turns`/`Inspect` ops are
  the structured surface for now.
- Forks *are* in the index — fork creation writes a `fork base` row and
  promote writes its own — but abandoned branches are derived only from
  rewind rows, so dropped forks still never show as abandoned. The
  conclusion holds; the stated reason did not.
- Hosts without a prompt hook get turn-less checkpoints (everything still
  works, `turn` is just null).
- The 1KB brief budget is enforced by dropping lines and truncating, so the
  summary is lossy under pressure rather than merely compact.

## Open questions (not yet settled)

1. **MCP-only hosts get no turn-linked history at all.** Claude Desktop,
   VS Code and OpenCode have no prompt hook, so the launch's headline feature
   — and the compliance story built on it — is absent exactly where the
   adapter is thinnest. The status note records this as a caveat; it is
   really a product question. *No lean.*
2. **The audit-log export format is still a follow-up**, while
   `07-compliance.md` and the README have both advertised change-management
   audit as a posture item. *Current lean: either design the export or stop
   claiming the audit log.*
3. **Prompt excerpts are persisted unconditionally** at 240 bytes, collapsed
   to a single line, in SQLite under the store root. That is a compliance
   surface nobody opted into. *No lean — see question 4 in `07-compliance.md`.*
4. **The index has no schema version or export path.** Migrations are
   additive and in-place, with legacy-table rebuilds. *No lean.*

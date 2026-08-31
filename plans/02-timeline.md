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

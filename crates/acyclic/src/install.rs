//! `acyclic install <host>` — wires the adapter into the current repo.
//!
//! claude-code: merges hook entries into the repo's `.claude/settings.json`
//! and drops the `/rewind`, `/timeline`, and `/fork` commands and the
//! self-rollback and fork-decompose skills into `.claude/`.
//! Checked-in files, so the whole team inherits the wiring.
//! agents-md: appends the CLI cheatsheet block to AGENTS.md for any
//! shell-capable agent.

use std::path::Path;

use serde_json::{json, Value};

pub fn run(repo: &Path, host: &str) -> Result<(), String> {
    match host {
        "claude-code" => claude_code(repo),
        "agents-md" | "--agents-md" => agents_md(repo),
        other => Err(format!(
            "unknown host {other:?} (expected claude-code | agents-md)"
        )),
    }
}

fn claude_code(repo: &Path) -> Result<(), String> {
    let claude_dir = repo.join(".claude");
    std::fs::create_dir_all(claude_dir.join("commands")).map_err(stringify)?;
    std::fs::create_dir_all(claude_dir.join("skills/acyclic-self-rollback")).map_err(stringify)?;
    std::fs::create_dir_all(claude_dir.join("skills/acyclic-fork-decompose")).map_err(stringify)?;

    merge_hooks(&claude_dir.join("settings.json"))?;
    std::fs::write(claude_dir.join("commands/rewind.md"), REWIND_COMMAND).map_err(stringify)?;
    std::fs::write(claude_dir.join("commands/timeline.md"), TIMELINE_COMMAND)
        .map_err(stringify)?;
    std::fs::write(
        claude_dir.join("skills/acyclic-self-rollback/SKILL.md"),
        SELF_ROLLBACK_SKILL,
    )
    .map_err(stringify)?;
    std::fs::write(claude_dir.join("commands/fork.md"), FORK_COMMAND).map_err(stringify)?;
    std::fs::write(
        claude_dir.join("skills/acyclic-fork-decompose/SKILL.md"),
        FORK_DECOMPOSE_SKILL,
    )
    .map_err(stringify)?;

    println!(
        "claude-code adapter installed into {}",
        claude_dir.display()
    );
    println!("  hooks:    .claude/settings.json (pre/post tool, prompt, session)");
    println!("  commands: /rewind, /timeline, /fork");
    println!("  skills:   acyclic-self-rollback, acyclic-fork-decompose");
    println!("check these files in so the whole team inherits checkpointing.");
    Ok(())
}

/// Merges our hook entries into settings.json without disturbing anything
/// else in the file. Idempotent: an entry whose command mentions
/// `acyclic hook` is replaced, never duplicated.
fn merge_hooks(settings_path: &Path) -> Result<(), String> {
    let mut settings: Value = match std::fs::read_to_string(settings_path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|error| format!("{}: {error}", settings_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.to_string()),
    };

    let events: [(&str, Option<&str>, &str); 5] = [
        (
            "PreToolUse",
            Some("Edit|Write|MultiEdit|NotebookEdit|Bash"),
            "acyclic hook pre-tool",
        ),
        (
            "PostToolUse",
            Some("Edit|Write|MultiEdit|NotebookEdit|Bash"),
            "acyclic hook post-tool",
        ),
        ("UserPromptSubmit", None, "acyclic hook user-prompt"),
        ("SessionStart", None, "acyclic hook session-start"),
        ("SessionEnd", None, "acyclic hook session-end"),
    ];

    let hooks = settings
        .as_object_mut()
        .ok_or("settings.json is not an object")?
        .entry("hooks")
        .or_insert(json!({}));
    let hooks = hooks.as_object_mut().ok_or("hooks is not an object")?;

    for (event, matcher, command) in events {
        let entries = hooks.entry(event).or_insert(json!([]));
        let entries = entries
            .as_array_mut()
            .ok_or_else(|| format!("hooks.{event} is not an array"))?;
        entries.retain(|entry| !is_ours(entry));
        let mut entry = json!({
            "hooks": [{ "type": "command", "command": command }]
        });
        if let Some(matcher) = matcher {
            entry["matcher"] = json!(matcher);
        }
        entries.push(entry);
    }

    let text = serde_json::to_string_pretty(&settings).map_err(stringify)?;
    std::fs::write(settings_path, text + "\n").map_err(stringify)?;
    Ok(())
}

/// An entry is ours iff one of its commands invokes `acyclic hook`.
/// Structural, not substring-over-JSON: a user hook that merely mentions
/// the phrase in an argument is left alone.
fn is_ours(entry: &Value) -> bool {
    entry["hooks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|hook| hook["command"].as_str())
        .any(|command| command == "acyclic" || command.starts_with("acyclic hook"))
}

fn agents_md(repo: &Path) -> Result<(), String> {
    let path = repo.join("AGENTS.md");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.contains("## Acyclic checkpoints") {
        println!("AGENTS.md already carries the acyclic block");
        return Ok(());
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(AGENTS_MD_BLOCK);
    std::fs::write(&path, content).map_err(stringify)?;
    println!("acyclic block appended to {}", path.display());
    Ok(())
}

fn stringify<E: std::fmt::Display>(error: E) -> String {
    error.to_string()
}

const REWIND_COMMAND: &str = r#"---
description: Restore the working tree to an earlier checkpoint (untracked and gitignored files included)
---

Restore this repo to an earlier acyclic checkpoint. Follow exactly:

1. Run `acyclic timeline` and show the user the recent checkpoints.
2. Ask which checkpoint to restore (or confirm if they already named one:
   $ARGUMENTS).
3. Run `acyclic rewind <id> --yes`.
4. Run `cd "$PWD"` in your shell. A rewind swaps the repo directory, and a
   shell that stays in the old inode silently runs every later command in
   the replaced tree.
5. Tell the user: the tree is restored exactly (including generated and
   gitignored files); a safety checkpoint of the pre-rewind state was taken
   automatically; and they should reload their editor, since open buffers
   still show the replaced tree.
6. Do not run any other write operations until the rewind completes.
"#;

const TIMELINE_COMMAND: &str = r#"---
description: Show the repo's history as conversation turns, diff what a turn changed, or restore one file from an earlier point
---

Answer the user's history question with acyclic's timeline. Request:
$ARGUMENTS

Pick the matching step:

- "What happened / what did each turn do": run `acyclic turns` (add
  `--session <id>` for an older session from `acyclic sessions`) and
  summarize which prompt led to which checkpoints.
- "What did turn N change" or "what did approach X look like": run
  `acyclic diff --turn N` and list the files; for an abandoned attempt the
  session brief names its checkpoint range, so `acyclic diff <a> <b>`.
- "Why did file X change": run `acyclic timeline --limit 200`, then
  `acyclic show <id>` on the checkpoint after the change to get the turn
  and the prompt that caused it.
- "Bring back just file X from earlier": `acyclic restore <id> <path>`.
  Only that path changes; the rest of the tree is untouched. Tell the user
  a checkpoint recorded the restore, so it is itself undoable.

Never guess at history from memory when the timeline can answer exactly.
"#;

const SELF_ROLLBACK_SKILL: &str = r#"---
name: acyclic-self-rollback
description: Use acyclic checkpoints to try risky changes safely - checkpoint before an attempt, rewind cleanly on failure instead of hand-reverting, and show a blast-radius diff before finishing. Use when a task is risky (migrations, refactors, codegen, dependency changes), when a failed attempt needs undoing, or before declaring multi-file work done.
---

# Self-rollback with acyclic

This repo checkpoints automatically around your tool calls (an acyclic
daemon snapshots the working tree, including untracked and gitignored
files). You can also use it deliberately:

## Before a risky attempt
Run `acyclic checkpoint --wait -m "before <attempt>"`. Note the printed
checkpoint id.

## When an attempt fails
Do NOT hand-revert edits (more edits poison the tree). Instead:
1. `acyclic timeline` - find the checkpoint from before the attempt.
2. `acyclic rewind <id> --yes` - the tree is back exactly, including
   generated files and Bash side effects.
3. Try the next approach from the clean state.

## Before declaring work done
Run `acyclic diff --stat` and review the blast radius: every file the
session changed, including what scripts and generators wrote. Mention
anything unexpected to the user.

## History across the conversation
Every checkpoint is linked to the conversation turn (prompt) that caused
it, and history survives across sessions:
- `acyclic turns` - which prompt led to which checkpoints.
- `acyclic diff --turn N` - exactly what turn N changed.
- `acyclic show <id>` - a checkpoint's session, turn, and prompt.
- `acyclic restore <id> <path>` - bring back ONE file from any checkpoint
  (an earlier abandoned approach, say) without touching the rest.
The session-start brief in your context names the previous session's end
checkpoint and any abandoned branches; use their ids directly.

## Rules
- After ANY `acyclic rewind` or `acyclic promote`, run `cd "$PWD"` before
  the next command. Both swap the repo directory; a shell left in the old
  inode silently operates on the replaced tree.
- Rewind restores file contents and modes, not mtimes: expect rebuilds.
- After a rewind, the user's editor may show stale buffers - say so.
- If `acyclic` reports the daemon is not running, checkpointing is off;
  tell the user to run `acyclic init` rather than working around it.
"#;


const FORK_COMMAND: &str = r#"---
description: Race N approaches to a task in isolated forks and land the winner, or run one risky step in a fork first
---

Use the acyclic-fork-decompose skill for this request. Arguments: $ARGUMENTS

Parse the arguments for parameter overrides in the form `key=value`
(fan_out, max_depth, max_forks, require_tests, test_command, tie_break);
everything else is the task. Run `acyclic policy` for the defaults, apply
the overrides, and state the effective parameters in one line before
forking.
"#;

const FORK_DECOMPOSE_SKILL: &str = r#"---
name: acyclic-fork-decompose
description: Solve a large or uncertain task by racing alternative approaches in isolated acyclic forks, landing one winner, and repeating from the new tree. Use when two or more designs are plausible, when a step must not touch the real tree until it is proven, or when the user runs /fork. Not for small edits: checkpoint and do those in place.
---

# Fork decomposition with acyclic

You are the ROOT agent in the real repository. Only the root can fork.
Subagents work inside fork directories and NEVER run `acyclic`. Recursion
is you repeating ROUND: fork, race, promote one, re-fork from the new tree.

## 0. Parameters (read first, every time)

Run `acyclic policy`. It prints:

```
fan_out = N          forks per race
max_depth = N        rounds before you must check in with the user
max_forks = N        total forks for this task
require_tests = bool a fork can win only if its tests pass
test_command = ...   command each subagent runs, or "(infer from the repo)"
tie_break = ...      "smallest-diff" | "first-passing" | "ask-user"
```

`/fork` arguments of the form `key=value` override these. State the
effective values in ONE line before the first fork, e.g.
`fork policy: fan_out=3 max_depth=2 max_forks=8 tests=required tie=smallest-diff`.

## 1. Decide ONCE, locally: DO, RACE, or SEQUENCE

- **DO** — one obvious approach, or a handful of edits. Run
  `acyclic checkpoint -m "before <task>"` and work in the real tree.
  If unsure, prefer DO. Rewind already makes a wrong turn cheap; a fork
  must beat rewind, not beat nothing.
- **RACE** — one goal, 2 or more genuinely different approaches, and a
  wrong choice would be expensive. Fork `fan_out` ways, one approach each.
- **SEQUENCE** — several dependent steps where a later step must not start
  until an earlier one is proven. One fork per step, promote, re-fork.

Never PARTITION (split into parts and land all of them). Promote requires
an unmoved mainline: after the first fork lands, every sibling's promote
reports "the working tree moved past the fork's base". Land ONE fork per
round.

## 2. One ROUND

Depth starts at 1 and increases by one per round.

1. `acyclic checkpoint -m "round <depth>: before <goal>"`.
2. `acyclic fork -n <fan_out>`. Note each id and path and whether it says
   `(mount)` or `(copy)`. Copy forks cost time proportional to the tree:
   keep them few and short-lived.
3. Dispatch ALL subagents in one turn, one per fork, using the CHILD
   PROMPT below. Do not keep one approach for yourself.
4. **Freeze.** Make NO edits to the real tree while forks are live. A
   single edit moves the mainline and every promote will conflict.
5. When all reports are in, `acyclic fork-diff <id>` for each fork.
6. Pick the winner by the SELECTION RULE. `acyclic promote <winner>`.
7. Run `cd "$PWD"`. Promote swaps the repo directory; a shell left in the
   old inode silently runs every later command in the replaced tree.
8. `acyclic fork-drop <id>` for every loser, immediately.
9. Report the round in the ROUND REPORT shape.
10. If work remains and depth < max_depth and forks used < max_forks,
    start the next round from the promoted tree. Otherwise stop and
    report to the user.

## 3. CHILD PROMPT (copy exactly, fill the angle brackets)

```
You are working in an isolated fork of the repository at:
  <absolute fork path>
Use ONLY absolute paths under that directory. Do not cd elsewhere. Do not
run any `acyclic` command: this directory is a fork, not the repository.
Do not read or write the real repository at <absolute repo root>.

GOAL: <one sentence, verbatim from the task>
YOUR APPROACH (<kebab-case-id>): <one or two sentences naming this
approach and what makes it different from the alternatives>
DEPTH: <depth> of <max_depth>

Do the work. Make your first tool call a write to the file you own most.
Before reporting, run the tests with the fork directory as the working
directory, e.g. `cd <absolute fork path> && <test_command>`:
  <test_command, or: infer the project's test command and run it>

Reply with exactly this shape and nothing else:

## APPROACH: <kebab-case-id>
### RESULT: PASS | FAIL | PARTIAL
### TESTS: <command run> -> <pass/fail count or "not run: reason">
### FILES: <one path per line, absolute path stripped to repo-relative>
### RISKS: <one line each, or "none">
### RECOMMEND: <one line: land | drop | needs <what>>
```

## 4. SELECTION RULE

1. Discard any fork whose RESULT is FAIL, and any with RESULT PARTIAL
   unless every fork is PARTIAL.
2. If `require_tests` is true, discard any fork whose TESTS did not pass.
3. If none remain: drop all forks, run ONE more round with the failures
   named in each child's approach text, then stop and report. Never
   promote a partial without saying so in the report.
4. If several remain, apply `tie_break`:
   - `smallest-diff`: fewest paths in `acyclic fork-diff`, then fewest RISKS.
   - `first-passing`: the first fork id in dispatch order that passed.
   - `ask-user`: show each candidate's fork-diff and RISKS, ask, wait.
5. A fork must be clearly better than doing nothing. If the winner's
   fork-diff is empty, drop it and treat the round as DO.

## 5. ROUND REPORT (to the user, after every round)

```
round <depth>/<max_depth> · <goal>
  winner  <id> <approach-id>  tests <result>  <N> paths
  dropped <id> <approach-id>  <one-line reason>
  (one dropped line per loser)
  forks used <n>/<max_forks>
```

After the final round: `acyclic diff <first checkpoint> <latest>` and
summarise the blast radius, ignoring `m` (metadata-only) lines. Mention
anything a script or generator wrote.

## 6. Failure and fallback

- A conflict on promote means the mainline moved: you edited the real
  tree, or a hook did. The conflicting fork is discarded by the engine,
  its changes included. Drop the remaining forks and re-run the round;
  do not try to salvage the conflicting one.
- `acyclic forks` says none are live after a daemon restart: every fork is
  lost. Re-run the round; nothing was landed.
- `acyclic fork` fails with a mounts error: read `acyclic status`, tell the
  user what it says, and fall back to DO.
- Two rounds in a row with no winner: stop and ask the user.

## Quick decision reference

| Situation | Action |
|---|---|
| One obvious approach, or a handful of edits | DO |
| 2+ genuinely different designs, expensive to guess wrong | RACE |
| Dependent steps, each must be proven first | SEQUENCE |
| Independent parts you want to land together | Not supported: SEQUENCE them |
| depth == max_depth and work remains | Stop, report, ask |
| forks used == max_forks | Stop, report, ask |
| Winner's fork-diff is empty | Drop it; the round was DO |
"#;

const AGENTS_MD_BLOCK: &str = r#"
## Acyclic checkpoints

This repo uses acyclic: the working tree (untracked + gitignored files
included) is snapshotted by a local daemon. Useful commands:

    acyclic checkpoint --wait -m "msg"   snapshot now, note the id
    acyclic timeline                     recent checkpoints
    acyclic rewind <id> --yes            restore the tree exactly
    acyclic diff --stat                  everything changed this session
    acyclic turns                        which prompt caused which checkpoints
    acyclic diff --turn N                what one conversation turn changed
    acyclic restore <id> <path>          bring back one file, leave the rest
    acyclic brief                        where the previous session ended

Before a risky change, checkpoint. After a failed attempt, rewind instead
of hand-reverting. Before finishing, review `acyclic diff`.
"#;
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_preserves_user_settings_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{
              "permissions": {"allow": ["Bash(ls:*)"]},
              "hooks": {
                "PreToolUse": [
                  {"matcher": "Bash",
                   "hooks": [{"type": "command", "command": "echo acyclic hook mention"}]}
                ]
              }
            }"#,
        )
        .expect("seed");

        merge_hooks(&settings).expect("first merge");
        merge_hooks(&settings).expect("second merge");

        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).expect("read")).expect("json");
        // User content survives — including a hook that merely MENTIONS the
        // phrase "acyclic hook" in an argument.
        assert_eq!(value["permissions"]["allow"][0], "Bash(ls:*)");
        let pre = value["hooks"]["PreToolUse"].as_array().expect("array");
        assert!(pre
            .iter()
            .any(|entry| { entry["hooks"][0]["command"] == "echo acyclic hook mention" }));
        // Exactly one of ours per event, no duplicates after re-install.
        let ours = |event: &str| {
            value["hooks"][event]
                .as_array()
                .expect("array")
                .iter()
                .filter(|entry| is_ours(entry))
                .count()
        };
        for event in [
            "PreToolUse",
            "PostToolUse",
            "UserPromptSubmit",
            "SessionStart",
            "SessionEnd",
        ] {
            assert_eq!(ours(event), 1, "{event}");
        }
    }

    #[test]
    fn merge_creates_settings_from_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = dir.path().join("settings.json");
        merge_hooks(&settings).expect("merge");
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).expect("read")).expect("json");
        assert_eq!(
            value["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "acyclic hook post-tool"
        );
        assert_eq!(
            value["hooks"]["PreToolUse"][0]["matcher"],
            "Edit|Write|MultiEdit|NotebookEdit|Bash"
        );
    }

    #[test]
    fn agents_md_appends_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("AGENTS.md"), "# Existing\n").expect("seed");
        agents_md(dir.path()).expect("first");
        agents_md(dir.path()).expect("second");
        let text = std::fs::read_to_string(dir.path().join("AGENTS.md")).expect("read");
        assert!(text.starts_with("# Existing\n"));
        assert_eq!(text.matches("## Acyclic checkpoints").count(), 1);
    }
}

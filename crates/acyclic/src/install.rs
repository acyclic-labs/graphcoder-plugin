//! `acyclic install <host>` — wires the adapter into the current repo.
//!
//! claude-code: merges hook entries into the repo's `.claude/settings.json`
//! and drops the `/rewind`, `/timeline`, and `/fork` commands and the
//! self-rollback and fork-decompose skills into `.claude/`.
//! codex: merges the same lifecycle hooks into `.codex/hooks.json` (Codex's
//! own event names and payload shape match Claude Code's closely enough
//! that `acyclic hook` needs no host-specific parsing) and appends the
//! agents-md cheatsheet, since Codex already reads AGENTS.md.
//! cursor: merges Cursor's differently-named agent hooks into
//! `.cursor/hooks.json` and drops an always-applied, product-named rule
//! file under `.cursor/rules/`; Cursor's payload shape (`conversation_id`
//! instead of `session_id`, no `tool_name` on shell hooks) is normalized in
//! `hook::Payload`. Also registers `acyclic mcp` as a project-scoped MCP
//! server at `.cursor/mcp.json`, since Cursor speaks MCP directly too.
//! Checked-in files, so the whole team inherits the wiring.
//! agents-md: appends the CLI cheatsheet block to AGENTS.md for any
//! shell-capable agent.
//! claude-desktop: no lifecycle-hook API exists, so this registers `acyclic
//! mcp` (an MCP stdio server; see `crate::mcp`) as an `mcpServers` entry in
//! the user's *global* `claude_desktop_config.json` instead of writing
//! anything under the repo — per-machine, not something a team can check in.
//! vscode: same shape as claude-desktop (no hook API, MCP is the only
//! extension point), but VS Code supports a project-scoped config file
//! (`.vscode/mcp.json`, different JSON shape — see `McpConfigShape`), so
//! this one IS checked-in like the hook-based adapters.
//!
//! Codex's MCP path (a fifth JSON-based option would be nice, but Codex's
//! MCP config is TOML) and Cursor/Codex desktop-vs-CLI hook parity are open
//! TODOs — see the doc comments on `codex()` and `cursor()`.

use acyclic_engine::product::{self, NAME};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

trait HostAdapter {
    fn id(&self) -> &'static str;
    fn install(&self, repo: &Path) -> Result<(), String>;
}

struct ClaudeCode;
struct Codex;
struct Cursor;
struct AgentsMd;

impl HostAdapter for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }
    fn install(&self, repo: &Path) -> Result<(), String> {
        claude_code(repo)
    }
}

impl HostAdapter for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn install(&self, repo: &Path) -> Result<(), String> {
        codex(repo)
    }
}

impl HostAdapter for Cursor {
    fn id(&self) -> &'static str {
        "cursor"
    }
    fn install(&self, repo: &Path) -> Result<(), String> {
        cursor(repo)
    }
}

impl HostAdapter for AgentsMd {
    fn id(&self) -> &'static str {
        "agents-md"
    }
    fn install(&self, repo: &Path) -> Result<(), String> {
        agents_md(repo)
    }
}

struct ClaudeDesktop;

impl HostAdapter for ClaudeDesktop {
    fn id(&self) -> &'static str {
        "claude-desktop"
    }
    fn install(&self, repo: &Path) -> Result<(), String> {
        claude_desktop(repo)
    }
}

struct VsCode;

impl HostAdapter for VsCode {
    fn id(&self) -> &'static str {
        "vscode"
    }
    fn install(&self, repo: &Path) -> Result<(), String> {
        vscode(repo)
    }
}

// TODO(more hosts): the two shapes this file already covers — lifecycle
// hooks (claude_code/codex/cursor) and MCP registration
// (claude_desktop/cursor/vscode) — generalize to most other coding-agent
// CLIs and desktop apps, not just the five above. Before adding one:
// 1. Find its hook config (file, event names, payload shape) and/or its MCP
//    config (file location, JSON vs TOML, top-level key, whether `type` is
//    explicit) from its own current docs — don't assume it matches an
//    existing adapter; VS Code alone differs from Claude Desktop/Cursor on
//    both the key name and the explicit-type requirement.
// 2. Prefer the MCP path when the config is JSON-shaped and matches (or is
//    close to) `McpConfigShape` — reuse `merge_mcp_server_json`, add a
//    shape constant and a `HostAdapter` impl, the same pattern as `vscode`.
// 3. Write the merge, add a unit test seeding an existing config to prove
//    other entries survive and a second install is idempotent (see
//    `vscode_install_writes_project_scoped_mcp_config`), then verify against
//    the real app before calling it more than "built."
// Concrete candidates, not yet done:
// - Kimi Code CLI: has BOTH lifecycle hooks (`[[hooks]]` in config.toml —
//   TOML again, like Codex) and MCP support (`kimi mcp` subcommands /
//   `/mcp-config`); unclear yet which config file MCP entries land in or
//   its exact JSON/TOML shape — check `kimi mcp` docs before writing.
// - Windsurf, Zed, JetBrains AI assistants, Gemini CLI, Amazon Q Developer:
//   unresearched. Likely MCP-capable (most 2026-era agent tools are) but
//   config location/shape unverified — do not assume any of them match
//   Claude Desktop/Cursor's shape without checking.
fn adapters() -> Vec<Box<dyn HostAdapter>> {
    vec![
        Box::new(ClaudeCode),
        Box::new(Codex),
        Box::new(Cursor),
        Box::new(AgentsMd),
        Box::new(ClaudeDesktop),
        Box::new(VsCode),
    ]
}

pub fn run(repo: &Path, host: &str) -> Result<(), String> {
    let host = if host == "--agents-md" {
        "agents-md"
    } else {
        host
    };
    adapters()
        .into_iter()
        .find(|adapter| adapter.id() == host)
        .ok_or_else(|| {
            let known: Vec<_> = adapters().iter().map(|adapter| adapter.id()).collect();
            format!("unknown host {host:?} (expected {})", known.join(" | "))
        })?
        .install(repo)
}

fn claude_code(repo: &Path) -> Result<(), String> {
    let claude_dir = repo.join(".claude");
    std::fs::create_dir_all(claude_dir.join("commands")).map_err(stringify)?;
    let rollback_skill = format!("skills/{NAME}-self-rollback");
    let decompose_skill = format!("skills/{NAME}-fork-decompose");
    std::fs::create_dir_all(claude_dir.join(&rollback_skill)).map_err(stringify)?;
    std::fs::create_dir_all(claude_dir.join(&decompose_skill)).map_err(stringify)?;

    merge_hooks(&claude_dir.join("settings.json"))?;
    std::fs::write(
        claude_dir.join("commands/rewind.md"),
        product::render(REWIND_COMMAND),
    )
    .map_err(stringify)?;
    std::fs::write(
        claude_dir.join("commands/timeline.md"),
        product::render(TIMELINE_COMMAND),
    )
    .map_err(stringify)?;
    std::fs::write(
        claude_dir.join(format!("{rollback_skill}/SKILL.md")),
        product::render(SELF_ROLLBACK_SKILL),
    )
    .map_err(stringify)?;
    std::fs::write(
        claude_dir.join("commands/fork.md"),
        product::render(FORK_COMMAND),
    )
    .map_err(stringify)?;
    std::fs::write(
        claude_dir.join(format!("{decompose_skill}/SKILL.md")),
        product::render(FORK_DECOMPOSE_SKILL),
    )
    .map_err(stringify)?;

    println!(
        "claude-code adapter installed into {}",
        claude_dir.display()
    );
    println!("  hooks:    .claude/settings.json (pre/post tool, prompt, session)");
    println!("  commands: /rewind, /timeline, /fork");
    println!("  skills:   {NAME}-self-rollback, {NAME}-fork-decompose");
    println!("check these files in so the whole team inherits checkpointing.");
    Ok(())
}

/// The five lifecycle events every adapter wires, and the matcher (tool
/// filter) and hook command each needs. Shared across hosts because Claude
/// Code and Codex use the same event names and payload shape; only the
/// container file's shape around this table differs. `host`, when it's
/// anything other than `acyclic hook`'s default (`claude-code`), is passed
/// through `ACYCLIC_HOST` so `session-start` records the right adapter and
/// Cursor's permission-controlled hooks get their required JSON reply.
fn hook_events(host: &str) -> [(&'static str, Option<&'static str>, String); 5] {
    let cmd = |verb: &str| {
        if host == "claude-code" {
            format!("{NAME} hook {verb}")
        } else {
            format!("ACYCLIC_HOST={host} {NAME} hook {verb}")
        }
    };
    [
        (
            "PreToolUse",
            Some("Edit|Write|MultiEdit|NotebookEdit|Bash"),
            cmd("pre-tool"),
        ),
        (
            "PostToolUse",
            Some("Edit|Write|MultiEdit|NotebookEdit|Bash"),
            cmd("post-tool"),
        ),
        ("UserPromptSubmit", None, cmd("user-prompt")),
        ("SessionStart", None, cmd("session-start")),
        ("SessionEnd", None, cmd("session-end")),
    ]
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

    let hooks = settings
        .as_object_mut()
        .ok_or("settings.json is not an object")?
        .entry("hooks")
        .or_insert(json!({}));
    let hooks = hooks.as_object_mut().ok_or("hooks is not an object")?;
    merge_event_hooks(hooks, "hooks", "claude-code")?;

    let text = serde_json::to_string_pretty(&settings).map_err(stringify)?;
    std::fs::write(settings_path, text + "\n").map_err(stringify)?;
    Ok(())
}

/// Merges our entries into a map keyed directly by event name (Codex's
/// `.codex/hooks.json` shape — no enclosing `"hooks"` key). Same
/// idempotency contract as `merge_hooks`.
fn merge_event_hooks(
    events_map: &mut serde_json::Map<String, Value>,
    label: &str,
    host: &str,
) -> Result<(), String> {
    for (event, matcher, command) in hook_events(host) {
        let entries = events_map.entry(event).or_insert(json!([]));
        let entries = entries
            .as_array_mut()
            .ok_or_else(|| format!("{label}.{event} is not an array"))?;
        entries.retain(|entry| !is_ours(entry));
        let mut entry = json!({
            "hooks": [{ "type": "command", "command": command }]
        });
        if let Some(matcher) = matcher {
            entry["matcher"] = json!(matcher);
        }
        entries.push(entry);
    }
    Ok(())
}

/// An entry is ours iff one of its commands invokes `acyclic hook`, with or
/// without our `ACYCLIC_HOST=<host>` env prefix (Codex/Cursor). Structural,
/// not substring-over-JSON: a user hook that merely mentions the phrase in
/// an argument is left alone.
fn is_ours(entry: &Value) -> bool {
    entry["hooks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|hook| hook["command"].as_str())
        .any(is_our_command)
}

fn is_our_command(command: &str) -> bool {
    let command = command
        .strip_prefix("ACYCLIC_HOST=")
        .and_then(|rest| rest.split_once(' '))
        .map_or(command, |(_, rest)| rest);
    command == NAME || command.starts_with(&format!("{NAME} hook"))
}

/// Codex CLI: `.codex/hooks.json` keyed directly by event name (no
/// enclosing `"hooks"` object, unlike Claude Code's settings.json), plus
/// the same host-neutral cheatsheet block Codex reads from AGENTS.md.
///
/// TODO(desktop/IDE parity): OpenAI's own docs state that the ChatGPT
/// desktop app, the Codex CLI, and Codex's IDE extension all read the same
/// `~/.codex/config.toml` / `.codex/config.toml`. If that also means they
/// share whatever fires `.codex/hooks.json`'s lifecycle events, this
/// adapter may already cover the desktop app and IDE extension too, with no
/// new code — verify that first. Only if hooks do NOT fire there does this
/// need an MCP path: Codex's MCP config lives in the same `config.toml`,
/// under TOML tables shaped `[mcp_servers.<name>]` with `command`/`args`/
/// `env` keys — a different format (TOML, not JSON) from every adapter in
/// this file, so it needs its own writer (and a `toml` crate dependency,
/// not currently in `Cargo.toml`) rather than reusing `merge_mcp_server_json`.
fn codex(repo: &Path) -> Result<(), String> {
    let codex_dir = repo.join(".codex");
    std::fs::create_dir_all(&codex_dir).map_err(stringify)?;
    merge_codex_hooks(&codex_dir.join("hooks.json"))?;
    agents_md(repo)?;

    println!("codex adapter installed into {}", codex_dir.display());
    println!("  hooks: .codex/hooks.json (pre/post tool, prompt, session)");
    println!("  AGENTS.md carries the {NAME} cheatsheet");
    println!("check these files in so the whole team inherits checkpointing.");
    Ok(())
}

fn merge_codex_hooks(hooks_path: &Path) -> Result<(), String> {
    let mut hooks: Value = match std::fs::read_to_string(hooks_path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|error| format!("{}: {error}", hooks_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.to_string()),
    };
    let events_map = hooks.as_object_mut().ok_or("hooks.json is not an object")?;
    merge_event_hooks(events_map, "hooks.json", "codex")?;

    let text = serde_json::to_string_pretty(&hooks).map_err(stringify)?;
    std::fs::write(hooks_path, text + "\n").map_err(stringify)?;
    Ok(())
}

/// Cursor: `.cursor/hooks.json` (schema version 1, entries carry `command`
/// and `type` directly rather than Claude/Codex's nested `hooks` array),
/// plus an always-applied project rule so the model has the CLI verbs in
/// context. Cursor's hook events and payload shape differ from Claude
/// Code/Codex (see `hook::Payload`'s `conversation_id`/`command` fallbacks
/// and `hook::run`'s Cursor-only `{"permission":"allow"}` reply), so this
/// writes Cursor's own event names rather than reusing `hook_events()`.
fn cursor(repo: &Path) -> Result<(), String> {
    let cursor_dir = repo.join(".cursor");
    std::fs::create_dir_all(cursor_dir.join("rules")).map_err(stringify)?;
    merge_cursor_hooks(&cursor_dir.join("hooks.json"))?;
    std::fs::write(
        cursor_dir.join(format!("rules/{NAME}.mdc")),
        product::render(CURSOR_RULE),
    )
    .map_err(stringify)?;

    // Cursor also speaks MCP directly, project-scoped and checked in
    // (unlike Claude Desktop's global-only config) — see
    // `merge_mcp_server_json`'s doc comment for the verified schema. Hooks
    // above already checkpoint automatically; this additionally exposes
    // named verbs (`checkpoint`, `rewind`, `timeline`, ...) as tools, the
    // same surface Claude Desktop and VS Code get.
    // TODO(verify): confirm in a real Cursor session that a hooks.json
    // adapter and an mcp.json server for the same product coexist cleanly
    // (expected: yes, they're independent request paths — a hook fires
    // automatically per tool call, an MCP tool call is model-initiated) and
    // that Cursor's desktop app fires the same hooks.json events its CLI
    // does before calling either path "supported" for the desktop app.
    let exe = std::env::current_exe().map_err(stringify)?;
    merge_mcp_server_json(&cursor_dir.join("mcp.json"), &MCP_SERVERS_SHAPE, &exe, repo)?;

    println!("cursor adapter installed into {}", cursor_dir.display());
    println!("  hooks: .cursor/hooks.json (shell + file-edit + prompt + session)");
    println!("  mcp:   .cursor/mcp.json ({NAME} tools, project-scoped)");
    println!("  rule:  .cursor/rules/{NAME}.mdc");
    println!("check these files in so the whole team inherits checkpointing.");
    Ok(())
}

fn merge_cursor_hooks(hooks_path: &Path) -> Result<(), String> {
    let mut root: Value = match std::fs::read_to_string(hooks_path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|error| format!("{}: {error}", hooks_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.to_string()),
    };
    let root_obj = root.as_object_mut().ok_or("hooks.json is not an object")?;
    root_obj.entry("version").or_insert(json!(1));
    let hooks = root_obj.entry("hooks").or_insert(json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or("hooks.hooks is not an object")?;

    // Cursor entries carry `command`/`type` directly (no nested `hooks`
    // array), so its own merge loop rather than `merge_event_hooks`.
    let events: [(&str, &str); 6] = [
        ("sessionStart", "session-start"),
        ("sessionEnd", "session-end"),
        ("beforeShellExecution", "pre-tool"),
        ("afterShellExecution", "post-tool"),
        ("afterFileEdit", "post-tool"),
        ("beforeSubmitPrompt", "user-prompt"),
    ];
    for (event, verb) in events {
        let entries = hooks.entry(event).or_insert(json!([]));
        let entries = entries
            .as_array_mut()
            .ok_or_else(|| format!("hooks.hooks.{event} is not an array"))?;
        entries.retain(|entry| !is_our_cursor_entry(entry));
        entries.push(json!({
            "type": "command",
            "command": format!("ACYCLIC_HOST=cursor {NAME} hook {verb}"),
        }));
    }

    let text = serde_json::to_string_pretty(&root).map_err(stringify)?;
    std::fs::write(hooks_path, text + "\n").map_err(stringify)?;
    Ok(())
}

fn is_our_cursor_entry(entry: &Value) -> bool {
    entry["command"].as_str().is_some_and(is_our_command)
}

/// Every JSON-based MCP host acyclic knows how to register `acyclic mcp`
/// with, verified against each host's own current docs (2026):
///
/// - Claude Desktop and Cursor: `{"mcpServers": {"<name>": {command, args}}}`
///   — identical shape, `stdio` inferred from the presence of `command`.
/// - VS Code (Copilot agent mode): `{"servers": {"<name>": {type, command,
///   args}}}` — different top-level key, and `type` must be explicit
///   (`"stdio"`); VS Code does not infer it the way the other two do.
///
/// Codex is deliberately absent: its MCP config is TOML
/// (`[mcp_servers.<name>]` in `config.toml`), not JSON, so it needs its own
/// writer — see the TODO on `codex()` below before adding one.
struct McpConfigShape {
    /// "mcpServers" (Claude Desktop, Cursor) or "servers" (VS Code).
    servers_key: &'static str,
    /// VS Code requires this; Claude Desktop and Cursor don't accept or need it.
    explicit_stdio_type: bool,
}

/// The `mcpServers` family: Claude Desktop and Cursor read the same shape.
const MCP_SERVERS_SHAPE: McpConfigShape = McpConfigShape {
    servers_key: "mcpServers",
    explicit_stdio_type: false,
};
const VSCODE_MCP_SHAPE: McpConfigShape = McpConfigShape {
    servers_key: "servers",
    explicit_stdio_type: true,
};

/// Merges an `acyclic mcp --repo <repo>` entry into any JSON-based MCP
/// host's config, preserving everything else — same read-modify-write
/// contract as `merge_hooks`: idempotent, keyed on the product name rather
/// than string-matching the whole file, so re-running `install` replaces
/// only this one entry.
fn merge_mcp_server_json(
    config_path: &Path,
    shape: &McpConfigShape,
    exe: &Path,
    repo: &Path,
) -> Result<(), String> {
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(stringify)?;
    }
    let mut config: Value = match std::fs::read_to_string(config_path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|error| format!("{}: {error}", config_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.to_string()),
    };
    let servers = config
        .as_object_mut()
        .ok_or_else(|| format!("{} is not an object", config_path.display()))?
        .entry(shape.servers_key)
        .or_insert(json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| format!("{} is not an object", shape.servers_key))?;
    let mut entry = json!({
        "command": exe.display().to_string(),
        "args": ["mcp", "--repo", repo.display().to_string()],
    });
    if shape.explicit_stdio_type {
        entry["type"] = json!("stdio");
    }
    servers.insert(NAME.to_string(), entry);

    let text = serde_json::to_string_pretty(&config).map_err(stringify)?;
    std::fs::write(config_path, text + "\n").map_err(stringify)?;
    Ok(())
}

/// Claude Desktop: unlike the repo-local adapters, there is no hook config
/// to drop — Desktop has no lifecycle-hook API, so `acyclic mcp` (an MCP
/// stdio server) is registered instead, in the user's *global*
/// `claude_desktop_config.json`. That file is per-machine, not something a
/// team can check in: each teammate who wants Desktop support runs this
/// locally once.
fn claude_desktop(repo: &Path) -> Result<(), String> {
    let config_path = claude_desktop_config_path()?;
    let exe = std::env::current_exe().map_err(stringify)?;
    merge_mcp_server_json(&config_path, &MCP_SERVERS_SHAPE, &exe, repo)?;

    println!(
        "claude-desktop adapter registered in {}",
        config_path.display()
    );
    println!("  server: {NAME} mcp --repo {}", repo.display());
    println!("per-machine, not checked into the repo: each teammate who wants");
    println!("Desktop support runs `{NAME} install claude-desktop` locally once.");
    println!("restart Claude Desktop for it to pick up the new server.");
    Ok(())
}

/// macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`.
/// Windows: `%APPDATA%\Claude\claude_desktop_config.json`. Linux:
/// `$XDG_CONFIG_HOME/Claude/claude_desktop_config.json`, falling back to
/// `~/.config/Claude/...`.
fn claude_desktop_config_path() -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
        Ok(PathBuf::from(home)
            .join("Library/Application Support/Claude/claude_desktop_config.json"))
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").map_err(|_| "APPDATA is not set".to_string())?;
        Ok(PathBuf::from(appdata).join("Claude/claude_desktop_config.json"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|home| PathBuf::from(home).join(".config"))
            });
        base.map(|base| base.join("Claude/claude_desktop_config.json"))
            .ok_or_else(|| "neither XDG_CONFIG_HOME nor HOME is set".to_string())
    }
}

/// VS Code (GitHub Copilot's agent mode): like Claude Desktop, there is no
/// lifecycle-hook API to drop repo-local config into — MCP is the only
/// extension point. Unlike Desktop, VS Code supports a workspace-local
/// config file (`.vscode/mcp.json`), so this one *is* checked-in, team-
/// shared config, same as the hook-based adapters.
///
/// TODO(verify): the schema below (`servers` key, explicit `"type":
/// "stdio"`) is confirmed against VS Code's current MCP docs but has not
/// been exercised against a real VS Code + Copilot agent-mode session. Test
/// that before calling this adapter "supported" rather than "built."
fn vscode(repo: &Path) -> Result<(), String> {
    let vscode_dir = repo.join(".vscode");
    let exe = std::env::current_exe().map_err(stringify)?;
    merge_mcp_server_json(&vscode_dir.join("mcp.json"), &VSCODE_MCP_SHAPE, &exe, repo)?;

    println!("vscode adapter installed into {}", vscode_dir.display());
    println!("  mcp: .vscode/mcp.json ({NAME} tools, project-scoped)");
    println!("check this file in so the whole team inherits it.");
    Ok(())
}

fn agents_md(repo: &Path) -> Result<(), String> {
    let path = repo.join("AGENTS.md");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.contains(&format!("## {NAME} checkpoints")) {
        println!("AGENTS.md already carries the {NAME} block");
        return Ok(());
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&product::render(AGENTS_MD_BLOCK));
    std::fs::write(&path, content).map_err(stringify)?;
    println!("{NAME} block appended to {}", path.display());
    Ok(())
}

fn stringify<E: std::fmt::Display>(error: E) -> String {
    error.to_string()
}

const REWIND_COMMAND: &str = r#"---
description: Restore the working tree to an earlier checkpoint (untracked and gitignored files included)
---

Restore this repo to an earlier {{name}} checkpoint. Follow exactly:

1. Run `{{name}} timeline` and show the user the recent checkpoints.
2. Ask which checkpoint to restore (or confirm if they already named one:
   $ARGUMENTS).
3. Run `{{name}} rewind <id> --yes`.
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

Answer the user's history question with {{name}}'s timeline. Request:
$ARGUMENTS

Pick the matching step:

- "What happened / what did each turn do": run `{{name}} turns` (add
  `--session <id>` for an older session from `{{name}} sessions`) and
  summarize which prompt led to which checkpoints.
- "What did turn N change" or "what did approach X look like": run
  `{{name}} diff --turn N` and list the files; for an abandoned attempt the
  session brief names its checkpoint range, so `{{name}} diff <a> <b>`.
- "Why did file X change": run `{{name}} timeline --limit 200`, then
  `{{name}} show <id>` on the checkpoint after the change to get the turn
  and the prompt that caused it.
- "Bring back just file X from earlier": `{{name}} restore <id> <path>`.
  Only that path changes; the rest of the tree is untouched. Tell the user
  a checkpoint recorded the restore, so it is itself undoable.

Never guess at history from memory when the timeline can answer exactly.
"#;

const SELF_ROLLBACK_SKILL: &str = r#"---
name: {{name}}-self-rollback
description: Use {{name}} checkpoints to try risky changes safely - checkpoint before an attempt, rewind cleanly on failure instead of hand-reverting, and show a blast-radius diff before finishing. Use when a task is risky (migrations, refactors, codegen, dependency changes), when a failed attempt needs undoing, or before declaring multi-file work done.
---

# Self-rollback with {{name}}

This repo checkpoints automatically around your tool calls (an {{name}}
daemon snapshots the working tree, including untracked and gitignored
files). You can also use it deliberately:

## Before a risky attempt
Run `{{name}} checkpoint --wait -m "before <attempt>"`. Note the printed
checkpoint id.

## When an attempt fails
Do NOT hand-revert edits (more edits poison the tree). Instead:
1. `{{name}} timeline` - find the checkpoint from before the attempt.
2. `{{name}} rewind <id> --yes` - the tree is back exactly, including
   generated files and Bash side effects.
3. Try the next approach from the clean state.

## Before declaring work done
Run `{{name}} diff --stat` and review the blast radius: every file the
session changed, including what scripts and generators wrote. Mention
anything unexpected to the user.

## History across the conversation
Every checkpoint is linked to the conversation turn (prompt) that caused
it, and history survives across sessions:
- `{{name}} turns` - which prompt led to which checkpoints.
- `{{name}} diff --turn N` - exactly what turn N changed.
- `{{name}} show <id>` - a checkpoint's session, turn, and prompt.
- `{{name}} restore <id> <path>` - bring back ONE file from any checkpoint
  (an earlier abandoned approach, say) without touching the rest.
The session-start brief in your context names the previous session's end
checkpoint and any abandoned branches; use their ids directly.

## Rules
- After ANY `{{name}} rewind` or `{{name}} promote`, run `cd "$PWD"` before
  the next command. Both swap the repo directory; a shell left in the old
  inode silently operates on the replaced tree.
- Rewind restores file contents and modes, not mtimes: expect rebuilds.
- Paths listed under `exclude` in .{{name}}/config.toml (secrets, bulky
  generated state) are never checkpointed: a rewind leaves them exactly
  as they are now, and `{{name}} restore` refuses them. Do not rely on a
  checkpoint to undo an edit to an excluded file.
- After a rewind, the user's editor may show stale buffers - say so.
- If `{{name}}` reports the daemon is not running, checkpointing is off;
  tell the user to run `{{name}} init` rather than working around it.
"#;

const FORK_COMMAND: &str = r#"---
description: Race N approaches to a task in isolated forks and land the winner, or run one risky step in a fork first
---

Use the {{name}}-fork-decompose skill for this request. Arguments: $ARGUMENTS

Parse the arguments for parameter overrides in the form `key=value`
(fan_out, max_depth, max_forks, require_tests, test_command, tie_break);
everything else is the task. Run `{{name}} policy` for the defaults, apply
the overrides, and state the effective parameters in one line before
forking.
"#;

const FORK_DECOMPOSE_SKILL: &str = r#"---
name: {{name}}-fork-decompose
description: Solve a large or uncertain task with isolated {{name}} forks - race alternative approaches and land one winner, partition independent parts across forks and land them all, or prove a risky step in a fork before it touches the real tree. Use when two or more designs are plausible, when a task splits into parts that touch different files, or when the user runs /fork. Not for small edits: checkpoint and do those in place.
---

# Fork decomposition with {{name}}

You are the ROOT agent in the real repository. Only the root can fork.
Subagents work inside fork directories and NEVER run `{{name}}`. Recursion
is you repeating ROUND: fork, race, promote one, re-fork from the new tree.

## 0. Parameters (read first, every time)

Run `{{name}} policy`. It prints:

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

## 1. Decide ONCE, locally: DO, RACE, PARTITION, or SEQUENCE

- **DO** — one obvious approach, or a handful of edits. Run
  `{{name}} checkpoint -m "before <task>"` and work in the real tree.
  If unsure, prefer DO. Rewind already makes a wrong turn cheap; a fork
  must beat rewind, not beat nothing.
- **RACE** — one goal, 2 or more genuinely different approaches, and a
  wrong choice would be expensive. Fork `fan_out` ways, one approach each,
  land ONE.
- **PARTITION** — independent parts. Fork one per part, land EVERY
  passing fork. Promote merges each fork onto the moved mainline: paths
  only that fork touched are written in place ("promoted by replay"), and
  a file both sides edited is merged three-way by content ("promoted by
  merge"). Only edits to the SAME LINES conflict: the promote then writes
  conflict markers into that fork and lands nothing; resolve them in the
  fork and promote again. ADJACENT lines count as the same region (a
  three-way merge cannot split one hunk), so a list of bullets or a
  block of imports is one region: give it to one fork. Assign ownership
  by file, or by region of a shared file, in each child prompt so
  conflicts stay rare. Edits other parts depend on (a type, an
  interface, a config key) go first as a SEQUENCE step, then partition
  the rest. Gitignored files (bytecode caches, build output, .env) never
  conflict: the mainline keeps its own copy and promote says so.
- **SEQUENCE** — several dependent steps where a later step must not start
  until an earlier one is proven. One fork per step, promote, re-fork.

## 2. One ROUND

Depth starts at 1 and increases by one per round.

1. `{{name}} checkpoint -m "round <depth>: before <goal>"`.
2. `{{name}} fork -n <fan_out>`. Note each id and path and whether it says
   `(mount)` or `(copy)`. Copy forks cost time proportional to the tree:
   keep them few and short-lived.
3. Dispatch ALL subagents in one turn, one per fork, using the CHILD
   PROMPT below. Do not keep one approach for yourself.
4. **Freeze.** Make NO edits to the real tree while forks are live. A
   real-tree edit merges like another fork would: fine when disjoint,
   a conflict to resolve when it touches the same lines.
5. When all reports are in, `{{name}} fork-diff <id>` for each fork.
6. RACE: pick the winner by the SELECTION RULE and `{{name}} promote <winner>`.
   PARTITION: `{{name}} promote <id>` for every fork that passed, in
   dispatch order. Every promote writes the fork's paths into the real
   tree in place; the repo directory is never replaced.
7. Read each promote's output: `promoted` / `promoted by merge` landed;
   `N file(s) conflict` wrote markers into that fork (see section 6).
8. `{{name}} fork-drop <id>` for every fork you will not land, immediately.
9. Report the round in the ROUND REPORT shape.
10. If work remains and depth < max_depth and forks used < max_forks,
    start the next round from the promoted tree. Otherwise stop and
    report to the user.

## 3. CHILD PROMPT (copy exactly, fill the angle brackets)

```
You are working in an isolated fork of the repository at:
  <absolute fork path>
Use ONLY absolute paths under that directory. Do not cd elsewhere. Do not
run any `{{name}}` command: this directory is a fork, not the repository.
Do not read or write the real repository at <absolute repo root>.

GOAL: <one sentence, verbatim from the task>
YOUR APPROACH (<kebab-case-id>): <one or two sentences naming this
approach and what makes it different from the alternatives>
YOU OWN: <PARTITION only: the files, directories, or regions of a shared
file this fork may change; touch nothing else, or your promote will
conflict with a sibling's>
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
   - `smallest-diff`: fewest paths in `{{name}} fork-diff`, not counting
     `m` (metadata) lines or `(gitignored)` lines, then fewest RISKS.
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

After the final round: `{{name}} diff <first checkpoint> <latest>` and
summarise the blast radius, ignoring `m` (metadata-only) lines. Mention
anything a script or generator wrote.

## 6. Failure and fallback

- A promote that reports `N file(s) conflict` has written conflict
  markers into THAT FORK (the mainline is untouched) and moved the fork
  onto the current tree. Open each named file in the fork, resolve every
  `<<<<<<<` / `|||||||` / `=======` / `>>>>>>>` block (the middle block is
  the original), leave no markers, then `{{name}} promote <id>` again. A
  `(deleted)` label means one side deleted the file: keep it without
  markers or delete it. `unresolved conflict markers` means a block is
  still there.
- A promote that says paths `cannot be merged` (binary, too large, a kind
  change, a directory deleted on one side and changed inside on the
  other) cannot be resolved in place: one fork must own that path. The
  fork is untouched; re-fork from the current tree and redo that part
  with tighter ownership.
- `{{name}} forks` says none are live after a daemon restart: every fork is
  lost. Re-run the round; nothing was landed.
- `{{name}} fork` fails with a mounts error: read `{{name}} status`, tell the
  user what it says, and fall back to DO.
- Two rounds in a row with no winner: stop and ask the user.

## Quick decision reference

| Situation | Action |
|---|---|
| One obvious approach, or a handful of edits | DO |
| 2+ genuinely different designs, expensive to guess wrong | RACE |
| Dependent steps, each must be proven first | SEQUENCE |
| Independent parts, different files | PARTITION |
| Independent parts that share a file, different regions | PARTITION; the merge is line-level |
| Independent parts that share lines, or a type/interface others use | SEQUENCE that edit first, then PARTITION |
| depth == max_depth and work remains | Stop, report, ask |
| forks used == max_forks | Stop, report, ask |
| Winner's fork-diff is empty | Drop it; the round was DO |
"#;

const AGENTS_MD_BLOCK: &str = r#"
## {{name}} checkpoints

This repo uses {{name}}: the working tree (untracked + gitignored files
included) is snapshotted by a local daemon. Useful commands:

    {{name}} checkpoint --wait -m "msg"   snapshot now, note the id
    {{name}} timeline                     recent checkpoints
    {{name}} rewind <id> --yes            restore the tree exactly
    {{name}} diff --stat                  everything changed this session
    {{name}} turns                        which prompt caused which checkpoints
    {{name}} diff --turn N                what one conversation turn changed
    {{name}} restore <id> <path>          bring back one file, leave the rest
    {{name}} brief                        where the previous session ended

Before a risky change, checkpoint. After a failed attempt, rewind instead
of hand-reverting. Before finishing, review `{{name}} diff`. Paths under
`exclude` in .{{name}}/config.toml are never checkpointed; a rewind leaves
them untouched.
"#;

const CURSOR_RULE: &str = r#"---
description: {{name}} checkpoints - use the CLI to inspect and restore history
alwaysApply: true
---

## {{name}} checkpoints

This repo uses {{name}}: the working tree (untracked + gitignored files
included) is snapshotted by a local daemon. Useful commands:

    {{name}} checkpoint --wait -m "msg"   snapshot now, note the id
    {{name}} timeline                     recent checkpoints
    {{name}} rewind <id> --yes            restore the tree exactly
    {{name}} diff --stat                  everything changed this session
    {{name}} turns                        which prompt caused which checkpoints
    {{name}} diff --turn N                what one conversation turn changed
    {{name}} restore <id> <path>          bring back one file, leave the rest
    {{name}} brief                        where the previous session ended

Before a risky change, checkpoint. After a failed attempt, rewind instead
of hand-reverting. Before finishing, review `{{name}} diff`. Paths under
`exclude` in .{{name}}/config.toml are never checkpointed; a rewind leaves
them untouched.
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
                   "hooks": [{"type": "command", "command": "echo NAME_HERE hook mention"}]}
                ]
              }
            }"#
            .replace("NAME_HERE", NAME),
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
            .any(|entry| { entry["hooks"][0]["command"] == format!("echo {NAME} hook mention") }));
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
            format!("{NAME} hook post-tool")
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
        assert_eq!(text.matches(&format!("## {NAME} checkpoints")).count(), 1);
    }

    #[test]
    fn codex_install_is_idempotent_and_writes_agents_md() {
        let dir = tempfile::tempdir().expect("tempdir");
        codex(dir.path()).expect("first install");
        codex(dir.path()).expect("second install");

        let hooks_path = dir.path().join(".codex/hooks.json");
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&hooks_path).expect("read"))
                .expect("json");
        for event in [
            "PreToolUse",
            "PostToolUse",
            "UserPromptSubmit",
            "SessionStart",
            "SessionEnd",
        ] {
            let entries = value[event].as_array().expect("array");
            assert_eq!(entries.iter().filter(|e| is_ours(e)).count(), 1, "{event}");
        }
        assert_eq!(
            value["SessionStart"][0]["hooks"][0]["command"],
            format!("ACYCLIC_HOST=codex {NAME} hook session-start")
        );

        let agents_md = std::fs::read_to_string(dir.path().join("AGENTS.md")).expect("read");
        assert_eq!(
            agents_md.matches(&format!("## {NAME} checkpoints")).count(),
            1
        );
    }

    #[test]
    fn cursor_install_is_idempotent_and_writes_rule() {
        let dir = tempfile::tempdir().expect("tempdir");
        cursor(dir.path()).expect("first install");
        cursor(dir.path()).expect("second install");

        let hooks_path = dir.path().join(".cursor/hooks.json");
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&hooks_path).expect("read"))
                .expect("json");
        assert_eq!(value["version"], 1);
        for event in [
            "sessionStart",
            "sessionEnd",
            "beforeShellExecution",
            "afterShellExecution",
            "afterFileEdit",
            "beforeSubmitPrompt",
        ] {
            let entries = value["hooks"][event].as_array().expect("array");
            assert_eq!(
                entries.iter().filter(|e| is_our_cursor_entry(e)).count(),
                1,
                "{event}"
            );
        }
        assert_eq!(
            value["hooks"]["beforeShellExecution"][0]["command"],
            format!("ACYCLIC_HOST=cursor {NAME} hook pre-tool")
        );

        let rule = std::fs::read_to_string(dir.path().join(format!(".cursor/rules/{NAME}.mdc")))
            .expect("read");
        assert!(rule.contains(&format!("## {NAME} checkpoints")));

        // Cursor also gets a project-scoped MCP server registration,
        // alongside its hooks — same verified schema as Claude Desktop.
        let mcp: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(".cursor/mcp.json")).expect("read"),
        )
        .expect("json");
        assert_eq!(
            mcp["mcpServers"][NAME]["command"],
            std::env::current_exe().unwrap().display().to_string()
        );
        assert_eq!(mcp["mcpServers"][NAME]["args"][0], "mcp");
    }

    #[test]
    fn vscode_install_writes_project_scoped_mcp_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        vscode(dir.path()).expect("first install");
        vscode(dir.path()).expect("second install (idempotent)");

        let value: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(".vscode/mcp.json")).expect("read"),
        )
        .expect("json");
        // VS Code's schema differs from Claude Desktop/Cursor on both the
        // top-level key ("servers", not "mcpServers") and requiring an
        // explicit "type" — see `merge_mcp_server_json`'s doc comment.
        assert_eq!(value["servers"][NAME]["type"], "stdio");
        assert_eq!(value["servers"][NAME]["args"][0], "mcp");
        assert!(value.get("mcpServers").is_none());
    }

    #[test]
    fn run_dispatches_known_hosts_and_rejects_unknown() {
        // claude-desktop is exercised separately (below): its install writes
        // to a global, per-machine config path, not anything under `repo`.
        for host in ["claude-code", "codex", "cursor", "agents-md", "vscode"] {
            let dir = tempfile::tempdir().expect("tempdir");
            run(dir.path(), host).unwrap_or_else(|error| panic!("{host}: {error}"));
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let error = run(dir.path(), "jetbrains").expect_err("unknown host");
        assert_eq!(
            error,
            "unknown host \"jetbrains\" (expected claude-code | codex | cursor | agents-md | claude-desktop | vscode)"
        );
    }

    #[test]
    fn claude_desktop_config_merge_is_idempotent_and_preserves_other_servers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("claude_desktop_config.json");
        std::fs::write(
            &config_path,
            r#"{
              "mcpServers": {
                "other-tool": {"command": "/usr/bin/other", "args": []}
              }
            }"#,
        )
        .expect("seed");

        let exe_path = format!("/usr/local/bin/{NAME}");
        let exe = std::path::Path::new(&exe_path);
        let repo = std::path::Path::new("/Users/dev/my-repo");
        merge_mcp_server_json(&config_path, &MCP_SERVERS_SHAPE, exe, repo).expect("first merge");
        merge_mcp_server_json(&config_path, &MCP_SERVERS_SHAPE, exe, repo)
            .expect("second merge (idempotent)");

        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).expect("read"))
                .expect("json");
        // The pre-existing server survives.
        assert_eq!(
            value["mcpServers"]["other-tool"]["command"],
            "/usr/bin/other"
        );
        // Exactly one acyclic entry, pointing at this exe and repo.
        assert_eq!(
            value["mcpServers"][NAME]["command"],
            exe.display().to_string()
        );
        assert_eq!(
            value["mcpServers"][NAME]["args"],
            json!(["mcp", "--repo", repo.display().to_string()])
        );
    }

    #[test]
    fn is_our_command_recognizes_host_prefixed_form() {
        assert!(is_our_command(&format!(
            "ACYCLIC_HOST=cursor {NAME} hook pre-tool"
        )));
        assert!(is_our_command(&format!("{NAME} hook pre-tool")));
        assert!(!is_our_command("echo not ours"));
        // A user command mentioning our phrase as an argument, not invoking
        // it, is left alone.
        assert!(!is_our_command(&format!("echo {NAME} hook mention")));
    }
}

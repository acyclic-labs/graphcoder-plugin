//! `acyclic install <host>` — wires one host's adapter into the current repo.
//!
//! Two adapter shapes. Hook-based hosts (claude-code, codex, cursor) get
//! lifecycle-hook config that calls `acyclic hook <event>` around every
//! edit and command, plus the commands/skills/rules that teach the agent
//! the verbs; all of it is checked-in, so the whole team inherits it.
//! MCP-based hosts (claude-desktop, vscode; cursor gets both) have no hook
//! API, so they get `acyclic mcp` (see `crate::mcp`) registered as an MCP
//! server in whatever config file that host reads — project-scoped and
//! checked in where the host supports it, the user's global config where
//! it doesn't. agents-md is the fallback for anything shell-capable: a
//! cheatsheet block in AGENTS.md and no hooks at all.
//!
//! Each `HostAdapter` below documents exactly what its host gets. The
//! README's per-host table is the user-facing version of the same list,
//! with how far each adapter has been verified.

use acyclic_engine::product::{self, NAME};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::hook::HookEvent;

/// Every host `acyclic install` knows. The CLI parses the kebab-case name
/// (`claude-code`, `agents-md`, ...) and rejects anything else before this
/// module sees it; the match in `run` is exhaustive, so adding a variant
/// without an installer is a compile error, not a runtime "unknown host".
#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
pub enum Host {
    ClaudeCode,
    Codex,
    Cursor,
    AgentsMd,
    ClaudeDesktop,
    #[value(name = "vscode")]
    VsCode,
    #[value(name = "opencode")]
    OpenCode,
}

// TODO(more hosts): the two shapes this file already covers — lifecycle
// hooks (claude_code/codex/cursor) and MCP registration
// (claude_desktop/cursor/vscode) — generalize to most other coding-agent
// CLIs and desktop apps, not just the ones below. Before adding one:
// 1. Find its hook config (file, event names, payload shape) and/or its MCP
//    config (file location, JSON vs TOML, top-level key, whether `type` is
//    explicit) from its own current docs — don't assume it matches an
//    existing adapter; VS Code alone differs from Claude Desktop/Cursor on
//    both the key name and the explicit-type requirement.
// 2. Prefer the MCP path when the config is JSON-shaped and matches (or is
//    close to) `McpConfigShape` — reuse `merge_mcp_server_json`, add a
//    shape constant, a `Host` variant, and its arm in `run`, the same
//    pattern as `vscode`.
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
pub fn run(repo: &Path, host: Host) -> Result<(), String> {
    match host {
        Host::ClaudeCode => claude_code(repo),
        Host::Codex => codex(repo),
        Host::Cursor => cursor(repo),
        Host::AgentsMd => agents_md(repo),
        Host::ClaudeDesktop => claude_desktop(repo),
        Host::VsCode => vscode(repo),
        Host::OpenCode => opencode(repo),
    }
}

fn claude_code(repo: &Path) -> Result<(), String> {
    let claude_dir = repo.join(".claude");
    std::fs::create_dir_all(claude_dir.join("commands")).map_err(stringify)?;
    let rollback_skill = format!("skills/{NAME}-self-rollback");
    let decompose_skill = format!("skills/{NAME}-fork-decompose");
    std::fs::create_dir_all(claude_dir.join(&rollback_skill)).map_err(stringify)?;
    std::fs::create_dir_all(claude_dir.join(&decompose_skill)).map_err(stringify)?;

    merge_hooks(&claude_dir.join("settings.json"), "claude-code")?;
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

/// Tools whose use changes the tree, so a checkpoint brackets them. Claude
/// Code's names plus Codex's `apply_patch` (its edit tool; shell commands
/// arrive as `Bash` there too). Hosts treat the matcher as a regex.
const MUTATING_TOOLS: &str = "Edit|Write|MultiEdit|NotebookEdit|Bash|apply_patch";

/// The five lifecycle events every adapter wires, and the matcher (tool
/// filter) and hook command each needs. Shared across hosts because Claude
/// Code and Codex use the same event names and payload shape; only the
/// container file's shape around this table differs. `host`, when it's
/// anything other than `acyclic hook`'s default (`claude-code`), is passed
/// through `ACYCLIC_HOST` so `session-start` records the right adapter and
/// Cursor's permission-controlled hooks get their required JSON reply.
fn hook_events(host: &str) -> [(&'static str, Option<&'static str>, String); 5] {
    let cmd = |event: HookEvent| {
        let verb = event.as_arg();
        if host == "claude-code" {
            format!("{NAME} hook {verb}")
        } else {
            format!("ACYCLIC_HOST={host} {NAME} hook {verb}")
        }
    };
    [
        ("PreToolUse", Some(MUTATING_TOOLS), cmd(HookEvent::PreTool)),
        (
            "PostToolUse",
            Some(MUTATING_TOOLS),
            cmd(HookEvent::PostTool),
        ),
        ("UserPromptSubmit", None, cmd(HookEvent::UserPrompt)),
        ("SessionStart", None, cmd(HookEvent::SessionStart)),
        ("SessionEnd", None, cmd(HookEvent::SessionEnd)),
    ]
}

/// Merges our hook entries into a `{"hooks": {<Event>: [...]}}` file
/// (Claude Code's `settings.json`, Codex's `hooks.json`) without disturbing
/// anything else in it. Idempotent: an entry whose command invokes
/// `acyclic hook` is replaced, never duplicated.
fn merge_hooks(path: &Path, host: &str) -> Result<(), String> {
    let mut root: Value = match std::fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.to_string()),
    };
    let root_map = root
        .as_object_mut()
        .ok_or_else(|| format!("{} is not an object", path.display()))?;
    if host == "codex" {
        remove_flat_codex_hooks(root_map);
    }
    let hooks = root_map.entry("hooks").or_insert(json!({}));
    let hooks = hooks.as_object_mut().ok_or("hooks is not an object")?;
    merge_event_hooks(hooks, "hooks", host)?;

    let text = serde_json::to_string_pretty(&root).map_err(stringify)?;
    write_atomic(path, &(text + "\n"))?;
    Ok(())
}

/// Earlier releases wrote Codex's events at the top level of `hooks.json`
/// (`{"PreToolUse": [...]}`), a shape Codex 0.154 silently ignores. Drop
/// our entries from that layout so a re-install moves them under `hooks`;
/// anything a user put there is left alone.
fn remove_flat_codex_hooks(root: &mut serde_json::Map<String, Value>) {
    for (event, _, _) in hook_events("codex") {
        let Some(entries) = root.get_mut(event).and_then(Value::as_array_mut) else {
            continue;
        };
        entries.retain(|entry| !is_ours(entry));
        if entries.is_empty() {
            root.remove(event);
        }
    }
}

/// Merges our entries into a map keyed by event name (the value under
/// `"hooks"`). Same idempotency contract as `merge_hooks`.
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
        if let (Some(matcher), Some(fields)) = (matcher, entry.as_object_mut()) {
            fields.insert("matcher".to_owned(), json!(matcher));
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
    // Word boundary after `hook`: `acyclic hookery ...` is somebody else's.
    command == NAME
        || command.starts_with(&format!("{NAME} hook "))
        || command == format!("{NAME} hook")
}

/// Codex CLI: `.codex/hooks.json`, whose shape is the same
/// `{"hooks": {<Event>: [{matcher, hooks: [{type, command}]}]}}` object as
/// Claude Code's settings.json (verified against Codex 0.154: a file with
/// events at the top level is ignored without a warning), plus the same
/// host-neutral cheatsheet block Codex reads from AGENTS.md. Codex asks the
/// user to trust project hooks once (`/hooks` in the TUI) before they run.
///
/// TODO(desktop/IDE parity): OpenAI's own docs state that the ChatGPT
/// desktop app, the Codex CLI, and Codex's IDE extension all read the same
/// `~/.codex/config.toml` / `.codex/config.toml`. If that also means they
/// share whatever fires `.codex/hooks.json`'s lifecycle events, this
/// adapter may already cover the desktop app and IDE extension too, with no
/// new code — verify that first. The MCP fallback is already known to
/// work (docs/manual-testing.md): `[mcp_servers.<name>]` with `command`,
/// `args` and `default_tools_approval_mode = "approve"` (without it a
/// non-interactive session rejects every call). It is TOML, so a writer
/// needs a `toml` dependency rather than `merge_mcp_server_json`.
fn codex(repo: &Path) -> Result<(), String> {
    let codex_dir = repo.join(".codex");
    std::fs::create_dir_all(&codex_dir).map_err(stringify)?;
    merge_hooks(&codex_dir.join("hooks.json"), "codex")?;
    agents_md(repo)?;

    println!("codex adapter installed into {}", codex_dir.display());
    println!("  hooks: .codex/hooks.json (pre/post tool, prompt, session)");
    println!("  AGENTS.md carries the {NAME} cheatsheet");
    println!("check these files in so the whole team inherits checkpointing.");
    println!("codex runs project hooks only after you trust them once: open `/hooks` in the TUI.");
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
    merge_mcp_server_json(
        &cursor_dir.join("mcp.json"),
        &MCP_SERVERS_SHAPE,
        &McpEntry::portable(),
    )?;

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
    let events: [(&str, HookEvent); 6] = [
        ("sessionStart", HookEvent::SessionStart),
        ("sessionEnd", HookEvent::SessionEnd),
        ("beforeShellExecution", HookEvent::PreTool),
        ("afterShellExecution", HookEvent::PostTool),
        ("afterFileEdit", HookEvent::PostTool),
        ("beforeSubmitPrompt", HookEvent::UserPrompt),
    ];
    for (event, hook_event) in events {
        let verb = hook_event.as_arg();
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
    write_atomic(hooks_path, &(text + "\n"))?;
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
/// writer — see `TODO(desktop/IDE parity)` on `codex()` before adding one.
struct McpConfigShape {
    /// "mcpServers" (Claude Desktop, Cursor) or "servers" (VS Code).
    servers_key: &'static str,
    /// The `type` field, where the host requires one: VS Code wants
    /// `stdio`, `OpenCode` wants `local`. Claude Desktop and Cursor neither
    /// accept nor need it.
    explicit_type: Option<&'static str>,
    /// `OpenCode` takes the binary and its arguments as ONE `command` array
    /// rather than separate `command` + `args` fields.
    command_as_array: bool,
    /// `OpenCode` gates each server on an explicit `enabled` flag.
    enabled_flag: bool,
    /// Written at the top level when the file is created, so editors can
    /// complete the config. Only `OpenCode` publishes one.
    schema_url: Option<&'static str>,
    /// VS Code documents a `cwd` field and substitutes `${workspaceFolder}`
    /// in it; Cursor's stdio schema has no `cwd`, and cursor-agent starts
    /// the server in the shell's cwd (verified: a subdirectory stays a
    /// subdirectory), so the server locates the root itself either way.
    workspace_cwd: bool,
}

/// The `mcpServers` family: Claude Desktop and Cursor read the same shape.
const MCP_SERVERS_SHAPE: McpConfigShape = McpConfigShape {
    servers_key: "mcpServers",
    explicit_type: None,
    command_as_array: false,
    enabled_flag: false,
    schema_url: None,
    workspace_cwd: false,
};
const VSCODE_MCP_SHAPE: McpConfigShape = McpConfigShape {
    servers_key: "servers",
    explicit_type: Some("stdio"),
    command_as_array: false,
    enabled_flag: false,
    schema_url: None,
    workspace_cwd: true,
};

/// `OpenCode`'s `opencode.json`: its own top-level key, `type: "local"`,
/// and the binary plus arguments as a single `command` array.
const OPENCODE_MCP_SHAPE: McpConfigShape = McpConfigShape {
    servers_key: "mcp",
    explicit_type: Some("local"),
    command_as_array: true,
    enabled_flag: true,
    schema_url: Some("https://opencode.ai/config.json"),
    workspace_cwd: false,
};

/// One `acyclic mcp` registration: the key it lives under and how the host
/// should launch it. Two flavours, because the two kinds of config file
/// have different readers:
///
/// - **Checked-in, per-project** (Cursor's `.cursor/mcp.json`, VS Code's
///   `.vscode/mcp.json`): every teammate's clone reads the same file, so it
///   must not carry this machine's paths. The command is the bare product
///   name, resolved on `PATH` exactly like the hook commands, and there is
///   no `--repo`: `acyclic mcp` walks up from its working directory to the
///   nearest initialized repo (`mcp::find_repo_root`), the way git finds
///   `.git`. `${workspaceFolder}` in `args` was tried first and rejected:
///   cursor-agent passes it through literally.
/// - **Per-machine, global** (Claude Desktop): the file is this user's own,
///   there is no workspace to start in, and one file serves every repo
///   this user registers, so the key carries the repo name and the args
///   carry the absolute paths.
struct McpEntry {
    key: String,
    command: String,
    /// `Some(path)` pins the server to one repo; `None` lets it find the
    /// root from its working directory.
    repo: Option<String>,
}

impl McpEntry {
    fn portable() -> Self {
        Self {
            key: NAME.to_owned(),
            command: NAME.to_owned(),
            repo: None,
        }
    }

    /// Keyed by the repo's directory name, which is what a user recognises
    /// in Desktop's server list; two registered repos that share a name get
    /// a short path-derived suffix so neither replaces the other.
    fn per_machine(exe: &Path, repo: &Path, taken: &serde_json::Map<String, Value>) -> Self {
        let basename = repo.file_name().map_or_else(
            || "repo".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        let repo_arg = repo.display().to_string();
        let plain = format!("{NAME}-{basename}");
        // A key is free when absent, or already ours for this very repo
        // (args are exactly `mcp --repo <this path>`; the command may have
        // moved). Anything else there (another repo, a hand-written server,
        // an entry with other args) is somebody's and must not be replaced,
        // so keep extending the candidate until one is free.
        let ours = json!(["mcp", "--repo", repo_arg]);
        let occupied = |key: &str| {
            taken
                .get(key)
                .is_some_and(|entry| entry.get("args") != Some(&ours))
        };
        let hashed = format!("{plain}-{:08x}", fnv1a(repo_arg.as_bytes()));
        let key = std::iter::once(plain)
            .chain(std::iter::once(hashed.clone()))
            .chain((2u32..).map(|n| format!("{hashed}-{n}")))
            .find(|candidate| !occupied(candidate))
            .unwrap_or(hashed);
        Self {
            key,
            command: exe.display().to_string(),
            repo: Some(repo_arg),
        }
    }

    fn args(&self) -> Value {
        match &self.repo {
            Some(repo) => json!(["mcp", "--repo", repo]),
            None => json!(["mcp"]),
        }
    }
}

/// FNV-1a over `bytes`: a stable, dependency-free short id for a path.
/// Not a security boundary, only a disambiguator for same-named repos.
fn fnv1a(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

/// The servers map a host config currently holds (empty when the file or
/// the key is absent), so a new entry can be keyed against what is there.
fn existing_servers(config_path: &Path, shape: &McpConfigShape) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(config_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|mut config| config.get_mut(shape.servers_key).map(Value::take))
        .and_then(|servers| servers.as_object().cloned())
        .unwrap_or_default()
}

/// Merges one `McpEntry` into any JSON-based MCP host's config, preserving
/// everything else — same read-modify-write contract as `merge_hooks`:
/// idempotent, keyed on the entry's key rather than string-matching the
/// whole file, so re-running `install` replaces only that one entry.
fn merge_mcp_server_json(
    config_path: &Path,
    shape: &McpConfigShape,
    entry: &McpEntry,
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
    let root = config
        .as_object_mut()
        .ok_or_else(|| format!("{} is not an object", config_path.display()))?;
    if let Some(url) = shape.schema_url {
        root.entry("$schema").or_insert(json!(url));
    }
    let servers = root.entry(shape.servers_key).or_insert(json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| format!("{} is not an object", shape.servers_key))?;
    // Re-install overwrites only the fields we own; anything the user added
    // to our entry (`env`, `envFile`, ...) survives.
    let existing = servers.remove(&entry.key);
    let mut value = existing
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    if let Some(fields) = value.as_object_mut() {
        if shape.command_as_array {
            // one array: the binary followed by its arguments
            let mut argv = vec![json!(entry.command)];
            if let Some(args) = entry.args().as_array() {
                argv.extend(args.iter().cloned());
            }
            fields.insert("command".to_owned(), Value::Array(argv));
            fields.remove("args");
        } else {
            fields.insert("command".to_owned(), json!(entry.command));
            fields.insert("args".to_owned(), entry.args());
        }
        if let Some(kind) = shape.explicit_type {
            fields.insert("type".to_owned(), json!(kind));
        }
        if shape.enabled_flag && !fields.contains_key("enabled") {
            fields.insert("enabled".to_owned(), json!(true));
        }
        if shape.workspace_cwd && entry.repo.is_none() {
            fields.insert("cwd".to_owned(), json!("${workspaceFolder}"));
        }
    }
    servers.insert(entry.key.clone(), value);

    let text = serde_json::to_string_pretty(&config).map_err(stringify)?;
    write_atomic(config_path, &(text + "\n"))
}

/// Write via a sibling temp file and rename, so a crash mid-write can never
/// leave a half-written config (Claude Desktop's is the user's whole MCP
/// server list, not just ours). The temp file is created exclusively, so a
/// planted symlink at the predictable name fails instead of being followed,
/// and it inherits the existing file's mode (a `0600` config with secrets
/// in `env` stays `0600`).
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let tmp = path.with_extension("json.tmp");
    // A stale temp file from an interrupted earlier run is ours to replace;
    // `remove_file` on a symlink removes the link, never the target.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", tmp.display())),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|error| format!("{}: {error}", tmp.display()))?;
    if let Ok(existing) = std::fs::metadata(path) {
        file.set_permissions(existing.permissions())
            .map_err(|error| format!("{}: {error}", tmp.display()))?;
    }
    file.write_all(text.as_bytes()).map_err(stringify)?;
    file.sync_all().map_err(stringify)?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(stringify)
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
    let repo = repo.canonicalize().map_err(stringify)?;
    let taken = existing_servers(&config_path, &MCP_SERVERS_SHAPE);
    let entry = McpEntry::per_machine(&exe, &repo, &taken);
    merge_mcp_server_json(&config_path, &MCP_SERVERS_SHAPE, &entry)?;

    println!(
        "claude-desktop adapter registered in {}",
        config_path.display()
    );
    println!(
        "  server: {} = {NAME} mcp --repo {}",
        entry.key,
        repo.display()
    );
    println!("  one entry per repo, keyed by directory name (a second repo with the same name");
    println!("  gets a short suffix): re-run this in another repo to add it alongside.");
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
        let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_owned())?;
        Ok(PathBuf::from(home)
            .join("Library/Application Support/Claude/claude_desktop_config.json"))
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").map_err(|_| "APPDATA is not set".to_owned())?;
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
            .ok_or_else(|| "neither XDG_CONFIG_HOME nor HOME is set".to_owned())
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
    merge_mcp_server_json(
        &vscode_dir.join("mcp.json"),
        &VSCODE_MCP_SHAPE,
        &McpEntry::portable(),
    )?;

    println!("vscode adapter installed into {}", vscode_dir.display());
    println!("  mcp: .vscode/mcp.json ({NAME} tools, project-scoped)");
    println!("check this file in so the whole team inherits it.");
    Ok(())
}

fn opencode(repo: &Path) -> Result<(), String> {
    // Project-scoped, checked in, and portable: a bare `acyclic` from PATH,
    // with the server locating the repo from its working directory.
    merge_mcp_server_json(
        &repo.join("opencode.json"),
        &OPENCODE_MCP_SHAPE,
        &McpEntry::portable(),
    )?;
    // OpenCode has no lifecycle-hook API, so nothing checkpoints around a
    // tool call; the cheatsheet plus the daemon's idle timer cover it.
    agents_md(repo)?;

    println!("opencode adapter installed into {}", repo.display());
    println!("  mcp:        opencode.json ({NAME} tools, project-scoped)");
    println!("  cheatsheet: AGENTS.md");
    println!("check both files in so the whole team inherits them.");
    println!("verify with: opencode mcp list");
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

## Splitting work across isolated forks

A fork is a writable copy-on-write view of the tree, cut in well under a
second however large the repo. Work inside one is invisible to the real
tree until you promote it.

    {{name}} policy                  the decomposition limits for this repo
    {{name}} fork -n N               cut N forks; each prints its path
    {{name}} fork-diff <id>          what one fork changed, before landing it
    {{name}} promote <id>            land that fork into the real tree
    {{name}} fork-drop <id>          discard it; its changes evaporate
    {{name}} forks                   what is live right now

Read `{{name}} policy` first and stay inside its limits. Then pick ONE:

- **DO** - one obvious approach, or a handful of edits. Checkpoint and work
  in the real tree. If unsure, prefer this: a fork must beat rewind, not
  beat nothing.
- **PARTITION** - independent parts touching different files. One fork per
  part, promote every fork that passed.
- **RACE** - one goal, 2+ genuinely different designs, expensive to guess
  wrong. One fork per approach, land exactly one.
- **SEQUENCE** - dependent steps. One fork per step, promote, re-fork.

Rules that are easy to get wrong:

- **Never fork a step that cannot be parallelised.** A summary, a merge, a
  reconciliation or a final ranking has no alternatives to race and nothing
  to split - forking it only adds coordination. Do it yourself.
- **Give each fork enough work to be worth it.** Coordination costs roughly
  a fixed amount per fork; if a fork's share is smaller than that, you are
  slower than doing it serially.
- **Freeze while forks are live.** Do not edit the real tree; a real-tree
  edit merges like another fork would.
- **Subagents never run `{{name}}`.** They work inside their fork directory
  by absolute path. Only you fork, promote and drop.
- A promote reporting `N file(s) conflict` wrote diff3 markers into THAT
  FORK and landed nothing. Resolve them there, then promote again.
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

## Splitting work across isolated forks

A fork is a writable copy-on-write view of the tree, cut in well under a
second however large the repo. Work inside one is invisible to the real
tree until you promote it.

    {{name}} policy                  the decomposition limits for this repo
    {{name}} fork -n N               cut N forks; each prints its path
    {{name}} fork-diff <id>          what one fork changed, before landing it
    {{name}} promote <id>            land that fork into the real tree
    {{name}} fork-drop <id>          discard it; its changes evaporate
    {{name}} forks                   what is live right now

Read `{{name}} policy` first and stay inside its limits. Then pick ONE:

- **DO** - one obvious approach, or a handful of edits. Checkpoint and work
  in the real tree. If unsure, prefer this: a fork must beat rewind, not
  beat nothing.
- **PARTITION** - independent parts touching different files. One fork per
  part, promote every fork that passed.
- **RACE** - one goal, 2+ genuinely different designs, expensive to guess
  wrong. One fork per approach, land exactly one.
- **SEQUENCE** - dependent steps. One fork per step, promote, re-fork.

Rules that are easy to get wrong:

- **Never fork a step that cannot be parallelised.** A summary, a merge, a
  reconciliation or a final ranking has no alternatives to race and nothing
  to split - forking it only adds coordination. Do it yourself.
- **Give each fork enough work to be worth it.** Coordination costs roughly
  a fixed amount per fork; if a fork's share is smaller than that, you are
  slower than doing it serially.
- **Freeze while forks are live.** Do not edit the real tree; a real-tree
  edit merges like another fork would.
- **Subagents never run `{{name}}`.** They work inside their fork directory
  by absolute path. Only you fork, promote and drop.
- A promote reporting `N file(s) conflict` wrote diff3 markers into THAT
  FORK and landed nothing. Resolve them there, then promote again.
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

        merge_hooks(&settings, "claude-code").expect("first merge");
        merge_hooks(&settings, "claude-code").expect("second merge");

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
        merge_hooks(&settings, "claude-code").expect("merge");
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).expect("read")).expect("json");
        assert_eq!(
            value["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            format!("{NAME} hook post-tool")
        );
        assert_eq!(value["hooks"]["PreToolUse"][0]["matcher"], MUTATING_TOOLS);
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
            let entries = value["hooks"][event].as_array().expect("array");
            assert_eq!(entries.iter().filter(|e| is_ours(e)).count(), 1, "{event}");
            assert!(value.get(event).is_none(), "{event} must not be top-level");
        }
        assert_eq!(
            value["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            format!("ACYCLIC_HOST=codex {NAME} hook session-start")
        );

        let agents_md = std::fs::read_to_string(dir.path().join("AGENTS.md")).expect("read");
        assert_eq!(
            agents_md.matches(&format!("## {NAME} checkpoints")).count(),
            1
        );
    }

    #[test]
    fn codex_reinstall_migrates_the_legacy_flat_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks_path = dir.path().join(".codex/hooks.json");
        std::fs::create_dir_all(dir.path().join(".codex")).expect("mkdir");
        let legacy = json!({
            "PreToolUse": [
                { "hooks": [{ "type": "command", "command": "echo user-hook" }] },
                { "hooks": [{ "type": "command",
                              "command": format!("ACYCLIC_HOST=codex {NAME} hook pre-tool") }] }
            ],
            "SessionEnd": [
                { "hooks": [{ "type": "command",
                              "command": format!("ACYCLIC_HOST=codex {NAME} hook session-end") }] }
            ]
        });
        std::fs::write(&hooks_path, legacy.to_string()).expect("seed");

        codex(dir.path()).expect("install");
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&hooks_path).expect("read"))
                .expect("json");
        assert_eq!(
            value["PreToolUse"].as_array().map(Vec::len),
            Some(1),
            "user hook kept"
        );
        assert!(
            value.get("SessionEnd").is_none(),
            "emptied legacy key removed"
        );
        assert_eq!(
            value["hooks"]["PreToolUse"].as_array().map(Vec::len),
            Some(1),
            "ours lives under hooks now"
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
        // Checked in, so portable: no machine paths, the binary comes from
        // PATH and the server finds the repo from its working directory.
        assert_eq!(mcp["mcpServers"][NAME]["command"], NAME);
        assert_eq!(mcp["mcpServers"][NAME]["args"], json!(["mcp"]));
        assert!(mcp["mcpServers"][NAME].get("cwd").is_none());
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
        assert_eq!(value["servers"][NAME]["command"], NAME);
        assert_eq!(value["servers"][NAME]["args"], json!(["mcp"]));
        assert_eq!(value["servers"][NAME]["cwd"], "${workspaceFolder}");
        assert!(value.get("mcpServers").is_none());
    }

    #[test]
    fn reinstall_keeps_user_added_fields_on_our_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        vscode(dir.path()).expect("first install");
        let path = dir.path().join(".vscode/mcp.json");
        let mut value: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        value["servers"][NAME]["env"] = json!({ "ACYCLIC_TRACE": "1" });
        std::fs::write(&path, value.to_string()).expect("seed user field");

        vscode(dir.path()).expect("second install");
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(value["servers"][NAME]["env"]["ACYCLIC_TRACE"], "1");
        assert_eq!(value["servers"][NAME]["args"], json!(["mcp"]));
    }

    #[test]
    fn every_host_installs_into_a_fresh_repo() {
        use clap::ValueEnum;
        // claude-desktop is exercised separately (below): its install writes
        // to a global, per-machine config path, not anything under `repo`.
        for host in Host::value_variants()
            .iter()
            .copied()
            .filter(|host| *host != Host::ClaudeDesktop)
        {
            let dir = tempfile::tempdir().expect("tempdir");
            run(dir.path(), host).unwrap_or_else(|error| panic!("{host:?}: {error}"));
        }
    }

    #[test]
    fn host_names_are_the_documented_kebab_case_ids() {
        use clap::ValueEnum;
        let names: Vec<String> = Host::value_variants()
            .iter()
            .map(|host| {
                host.to_possible_value()
                    .expect("named")
                    .get_name()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            names,
            [
                "claude-code",
                "codex",
                "cursor",
                "agents-md",
                "claude-desktop",
                "vscode",
                "opencode"
            ]
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
        let register = |repo: &Path| {
            let taken = existing_servers(&config_path, &MCP_SERVERS_SHAPE);
            let entry = McpEntry::per_machine(exe, repo, &taken);
            merge_mcp_server_json(&config_path, &MCP_SERVERS_SHAPE, &entry).expect("merge");
            entry.key
        };
        // Twice: the second install must be idempotent.
        register(repo);
        register(repo);
        // A second repo lands alongside, not on top of, the first.
        let other_repo = std::path::Path::new("/Users/dev/other-repo");
        register(other_repo);
        // A third repo with the same directory name as the first gets a
        // path-derived suffix instead of retargeting the first entry.
        let twin_repo = std::path::Path::new("/Users/dev/elsewhere/my-repo");
        let twin_key = register(twin_repo);
        assert_ne!(twin_key, format!("{NAME}-my-repo"));
        assert!(
            twin_key.starts_with(&format!("{NAME}-my-repo-")),
            "{twin_key}"
        );
        assert_eq!(register(twin_repo), twin_key, "the suffixed key is stable");
        // Even the suffixed key is checked: a hand-written server sitting on
        // it is left alone and the next candidate is used.
        let planted = {
            let mut config: Value =
                serde_json::from_str(&std::fs::read_to_string(&config_path).expect("read"))
                    .expect("json");
            let taken = existing_servers(&config_path, &MCP_SERVERS_SHAPE);
            let fourth = std::path::Path::new("/Users/dev/again/my-repo");
            let would_be = McpEntry::per_machine(exe, fourth, &taken).key;
            config["mcpServers"][&would_be] = json!({ "command": "/usr/bin/theirs", "args": [] });
            std::fs::write(&config_path, config.to_string()).expect("plant");
            let taken = existing_servers(&config_path, &MCP_SERVERS_SHAPE);
            let entry = McpEntry::per_machine(exe, fourth, &taken);
            merge_mcp_server_json(&config_path, &MCP_SERVERS_SHAPE, &entry).expect("merge");
            (would_be, entry.key)
        };
        assert_ne!(planted.0, planted.1, "occupied suffix skipped");

        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).expect("read"))
                .expect("json");
        // The pre-existing server survives.
        assert_eq!(
            value["mcpServers"]["other-tool"]["command"],
            "/usr/bin/other"
        );
        // Exactly one entry per repo, keyed by repo name, pointing at this
        // exe and that repo's absolute path.
        let key = format!("{NAME}-my-repo");
        assert_eq!(
            value["mcpServers"][&key]["command"],
            exe.display().to_string()
        );
        assert_eq!(
            value["mcpServers"][&key]["args"],
            json!(["mcp", "--repo", repo.display().to_string()])
        );
        assert_eq!(
            value["mcpServers"][format!("{NAME}-other-repo")]["args"][2],
            other_repo.display().to_string()
        );
        assert_eq!(
            value["mcpServers"][&twin_key]["args"][2],
            twin_repo.display().to_string()
        );
        assert_eq!(
            value["mcpServers"][&planted.0]["command"], "/usr/bin/theirs",
            "planted server untouched"
        );
        assert_eq!(
            value["mcpServers"][&planted.1]["args"][2],
            "/Users/dev/again/my-repo"
        );
        // other-tool, my-repo, other-repo, twin, planted, fourth.
        assert_eq!(value["mcpServers"].as_object().map(|m| m.len()), Some(6));
    }

    #[test]
    fn write_atomic_refuses_a_planted_symlink_and_keeps_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{}\n").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        // A symlink at the predictable temp name must not be followed.
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "keep me\n").expect("victim");
        std::os::unix::fs::symlink(&victim, path.with_extension("json.tmp")).expect("plant");
        write_atomic(&path, "{\"a\":1}\n").expect("write");
        assert_eq!(std::fs::read_to_string(&victim).expect("read"), "keep me\n");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "{\"a\":1}\n");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "existing mode preserved");
    }

    #[test]
    fn is_our_command_recognizes_host_prefixed_form() {
        assert!(is_our_command(&format!(
            "ACYCLIC_HOST=cursor {NAME} hook pre-tool"
        )));
        assert!(is_our_command(&format!("{NAME} hook pre-tool")));
        assert!(!is_our_command("echo not ours"));
        assert!(!is_our_command(&format!("{NAME} hookery --flag")));
        // A user command mentioning our phrase as an argument, not invoking
        // it, is left alone.
        assert!(!is_our_command(&format!("echo {NAME} hook mention")));
    }
}

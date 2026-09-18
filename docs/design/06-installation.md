# Installation & usage

One engine, thin adapters. Every capability lives in a single local engine — the `acyclic` CLI and its background daemon (watcher, snapshot store, index). Each coding agent gets a thin adapter wiring the engine into that host's native extension points. The engine is host-agnostic; only the adapter knows it's running inside Claude Code, Codex, or OpenCode. Any agent that can run shell commands can use the engine with no adapter at all.

## Install flow

```sh
npm i -g @acyclic-labs/plugin   # prebuilt binary, macOS + Linux + Windows x64
cd your-repo
acyclic init                    # starts the daemon, builds the first snapshot
acyclic install claude-code     # or: codex · cursor · agents-md · claude-desktop · vscode · opencode · copilot · copilot-agent
```

`acyclic install <host>` writes that host's adapter. It does **not** detect the
host — the name is required, and the dev has to know which of seven rows in the
README table describes their setup. That is our taxonomy leaking into their
first five minutes; see [Onboarding](#onboarding-proposed) for the fix.

From then on the dev starts their agent as usual — checkpointing is on.

## Distribution

The engine is one static binary per target. Getting it onto a machine is a
separate design problem from wiring it into a host, and it is currently the
weaker half.

### Channels

| Channel | State | Notes |
|---|---|---|
| **npm** `@acyclic-labs/plugin` | **live, 0.0.1** | A pure-JS launcher with four per-platform optional dependencies carrying the binary. `os`/`cpu` are declared, so npm installs exactly one. Needs Node ≥ 18. |
| `scripts/install.sh` via curl | written, **blocked** | Verified download into `~/.local/bin`, checked against the release's `SHA256SUMS`. Blocked on two gates below. |
| GitHub release asset, in a browser | **blocked** | Same two gates, plus Gatekeeper — see Signing. |
| Homebrew | **not built** | No formula, no tap, no release job. Was documented in this file as if it shipped. |
| crates.io `cargo install` | **not published** | The workspace is not published; `acyclic-fs` is a git dependency pinned to a SHA. |
| From source | works with access | The `acyclic-fs` repo is public; only this one is private. |

### The two gates

The standalone installer fails for two independent reasons, and cutting a
release only clears one:

1. **This repo is private.** `raw.githubusercontent.com/<repo>/main/scripts/install.sh`
   returns 404 to anyone outside the org, and release assets need a token too.
2. **No release has been tagged.** `git tag` and `gh release list` are both
   empty, so `releases/latest/download/` has nothing to resolve even with a token.

Both gates are about **GitHub**, not about the binaries. That distinction was
missed when this section was first written, and it matters.

### The binaries are already public

Measured 16 Sep 2026: the npm registry serves the platform packages
unauthenticated.

```
GET registry.npmjs.org/@acyclic-labs/plugin-darwin-arm64/-/plugin-darwin-arm64-0.0.1.tgz
→ 200, 5,504,945 bytes, package/bin/acyclic: Mach-O 64-bit executable arm64
```

The packument publishes `dist.integrity` as a sha512, and the verification
chain reproduces in POSIX shell — `curl` + `shasum -a 512` + `xxd -r -p` +
`base64` matched the published integrity exactly. That is the same property
`SHA256SUMS` provides on a GitHub release, from a host that is already public.

So a third channel shape exists that neither gate touches:

| Piece | From | State |
|---|---|---|
| the installer script | a public domain — `acyclic.dev` answers 200 at root | not hosted yet |
| the binaries | `registry.npmjs.org`, integrity-verified | already public |

This is worth taking seriously before treating "make the repo public" as the
prerequisite for a curl installer. It is not: npm is already a public,
integrity-checked CDN for exactly these artifacts, and has been since 0.0.1.
Untested end to end — each link is verified, the chain is not.

What the two gates still block is reading the source, the SLSA and SBOM
attestations (which live on the GitHub release), and the "verifiable open
source" claim in `07-compliance.md`. Those are real, and they are not the
same problem as getting a binary onto a laptop.

### Platform support

Four native targets, built on their own runners; no cross-compilation.

| Target | Floor |
|---|---|
| `darwin-arm64`, `darwin-x64` | No documented minimum; inherits Rust's default deployment target. |
| `linux-x64`, `linux-arm64` | **glibc ≥ 2.34** — dynamically linked `gnu` targets, no musl or static build. |

The glibc floor rules out Ubuntu 20.04, Debian 11, RHEL 8 and Amazon Linux 2.
It is enforced nowhere: npm filters on `os`/`cpu`, which it does correctly, but
has no concept of a libc version, so those distros install cleanly and hand the
user a binary that cannot start. `install.sh` does catch it — it runs
`--version` and fails with "installed binary does not run on this machine" —
so the channel we lead with is the one that fails latest and least clearly.

Windows has no native target. WSL2 is an ordinary Linux install and is expected
to work, but is untested and therefore unclaimed.

### Signing

There is no macOS code signing or notarization anywhere in the release
pipeline — no Developer ID, no `codesign`, no `notarytool`. The arm64 binary
carries only the ad-hoc signature the linker must emit for Apple Silicon to
execute it at all; the x64 binary is unsigned. Both are rejected by `spctl`.

Supply-chain trust is SLSA build provenance, an SPDX SBOM attested per binary,
and `SHA256SUMS` — all real, all verified in CI, and none of it consulted by
Gatekeeper. The gap is invisible today because neither live channel sets the
quarantine attribute: npm extracts from a tarball, and curl does not mark
downloads. It becomes visible the moment someone saves an asset from the
releases page in a browser.

Measured on macOS 26.5.1, the same binary run twice:

| | stdout / stderr | exit |
|---|---|---|
| with `com.apple.quarantine` | *empty* | **137** — SIGKILL |
| without | `acyclic 0.0.1` | 0 |

The failure mode is not the "developer cannot be verified" dialog a user can
click through. Executed from a shell, the process is killed outright with
nothing written to either stream. Anyone who downloads a release asset in a
browser and runs it gets silence and a non-zero status, with no indication
that code signing is the cause. Either we notarize, or we state that the
browser route is unsupported and keep people on npm.

## Onboarding (proposed)

Nothing in this section is built. It is the design for what `init` should do
once the binary is on the machine.

Today the dev runs `init`, then `install <host>` with the host named. The
decision they are asked to make first is one only we can see the answer to.

### The detection ladder

Every question answerable from the filesystem is a question not asked. Probes
are tried in order; the first that hits wins.

1. **What this repo already contains** — `.claude/`, `.codex/`, `.cursor/`,
   `opencode.json`, `AGENTS.md`. The strongest signal: it says what the *team*
   standardised on, is checked in, and survives a clone.
2. **What is on PATH** — `command -v claude / codex / cursor-agent / opencode`.
   Already prototyped in the demo kit's `harness_available`. Finds CLI hosts
   only, and a laptop with four agents installed says nothing about this repo.
3. **What is installed but has no CLI** — Claude Desktop and VS Code, found by
   their config locations. Weakest signal, because presence on the machine does
   not imply use on this repo, so this rung asks rather than assumes.

### The question budget

| Situation | Questions |
|---|---|
| One host detected, clean repo | 1 — a confirm, not a choice |
| Several hosts detected | 2 — which ones, then confirm the writes |
| Repo already wired | 0 — report and exit |
| Nothing detected (BYOH) | 3 — what are you running · does it speak MCP · does it read a rules file |
| Secrets found in the tree | +1, and only when something is actually found |

BYOH being the expensive path is correct: three questions is the price of
supporting an agent nobody wrote an adapter for, and the alternative is telling
someone their tool is unsupported.

### Where an unknown harness lands

The BYOH ladder descends through the three adapter shapes this design already
commits to, so an unknown host always terminates somewhere real:

1. **Lifecycle hooks** if the host has them — automatic checkpoints, no model
   cooperation, config checked in.
2. **MCP** if it speaks it — register `acyclic mcp`. For a host with no writer,
   print the JSON to paste rather than guessing at its config path.
3. **Shell only** — the AGENTS.md cheatsheet plus `auto_checkpoint_idle_ms`.
   This is a real floor, not a failure: the watcher sees every write whoever
   made it, so rewind and timeline work with zero host cooperation.
   `ACYCLIC_AGENT_CMD`, currently a demo-kit escape hatch, belongs here.

### The non-interactive contract

`install-smoke.sh`, every acceptance script, and any CI use must keep working.
So: every question needs an equivalent flag, a non-TTY takes the defaults
silently and never blocks, and `--yes` skips confirmation everywhere. An
interactive flow that cannot be driven headlessly is a regression, not a
feature.

## Per-host adapters

| Host | Mechanism |
|---|---|
| **Claude Code** | Native plugin: PreToolUse/PostToolUse hooks trigger checkpoints around edits and commands; slash commands (`/rewind`, `/fork`, `/timeline`) surface the engine to the dev; a skill teaches the agent the engine's verbs (self-rollback, fork-and-try, blast-radius diff). |
| **Codex** | `.codex/hooks.json` lifecycle hooks (`PreToolUse`/`PostToolUse`/`UserPromptSubmit`/`SessionStart`/`SessionEnd`) drive the same checkpointing as Claude Code — the file has the same `{"hooks": {...}}` shape as Claude Code's settings.json (Codex 0.154 ignores events written at the top level, which an earlier adapter did; re-install migrates them), and Codex's payload shape matches closely enough that `acyclic hook` needs no host-specific parsing. Codex runs project hooks only after the user trusts them once (`/hooks`). Engine verbs are taught via the same AGENTS.md block `--agents-md` writes. Codex also speaks MCP from the same `config.toml` the ChatGPT desktop app and IDE extension read; `acyclic mcp` works there with `default_tools_approval_mode = "approve"` on the server entry (verified via `-c` overrides, no writer yet). |
| **Cursor** | `.cursor/hooks.json` agent hooks (`beforeShellExecution`/`afterShellExecution`/`afterFileEdit`/`beforeSubmitPrompt`/`sessionStart`/`sessionEnd`); Cursor's payload uses `conversation_id` and a bare `command` string rather than Claude/Codex's `session_id`/`tool_name`, so `acyclic hook` falls back to those fields when present. An always-applied `.cursor/rules/<name>.mdc` (product-named, see `product.toml`) teaches the engine's verbs. Also registers `acyclic mcp` at a project-scoped `.cursor/mcp.json` (Cursor speaks MCP directly, same `mcpServers` shape as Claude Desktop below; the checked-in entry is portable: bare `acyclic` resolved on `PATH` and no `--repo`; the server walks up from its working directory to the initialized repo, since cursor-agent neither substitutes `${workspaceFolder}` nor starts the server at the workspace root) — hooks give automatic checkpointing, MCP additionally gives named tools the model can call explicitly. |
| **OpenCode** | Speaks MCP from a project-scoped `opencode.json` — its own top-level `mcp` key, `type: "local"`, and the binary plus arguments as ONE `command` array rather than `command` + `args`, with `enabled` and `$schema`. That shape matches neither existing family, so `McpConfigShape` grew `command_as_array`, `enabled_flag` and `schema_url` rather than the adapter growing a bespoke writer. `acyclic install opencode` also writes the AGENTS.md cheatsheet, since OpenCode has no lifecycle-hook API — nothing checkpoints around a tool call, so the cheatsheet plus the daemon's idle timer cover it. Verified against the real CLI: `opencode mcp list` reports the server connected. Its plugin system also has JS lifecycle hooks, which would give automatic checkpoints like the Claude Code adapter; that writer does not exist yet. |
| **Anything else** | `acyclic install --agents-md` drops an instructions block teaching any shell-capable agent the CLI. Degraded gracefully: no hook-triggered checkpoints, but watcher-driven ones still work. |
| **Claude Desktop** | No lifecycle-hook API exists, so there is nothing to hook into — `acyclic install claude-desktop` instead registers `acyclic mcp` (an MCP stdio server) as an `mcpServers.acyclic-<repo name>` entry in the user's global `claude_desktop_config.json`, one entry per registered repo (absolute binary and repo paths, since this file is the user's own); per-machine, not checked into the repo. The server exposes `checkpoint`/`timeline`/`rewind`/`diff`/`restore`/`turns`/`brief` as MCP tools, each translating directly into the engine's existing wire protocol. Checkpointing is not automatic on every tool call the way it is for a hooked host: it happens when the model calls `checkpoint`, or via the engine's `auto_checkpoint_idle_ms` idle timer once the watcher has pending changes — a real capability gap, not hidden by the tool descriptions that steer the model toward calling it. Shipped after the evaluation below; `.mcpb` packaging and a possibly-shared server are the next things to revisit, not blockers on today's flow. |
| **VS Code** (Copilot agent mode) | Same shape as Claude Desktop — no lifecycle-hook API, MCP is the only extension point — but VS Code supports a project-scoped `.vscode/mcp.json`, checked in like the hook-based adapters rather than global-only, so the entry is portable (bare `acyclic` on `PATH`, `cwd: ${workspaceFolder}`, and the server locates the repo from there). Different JSON shape from Claude Desktop/Cursor: top-level key is `servers` (not `mcpServers`), and every entry needs an explicit `"type": "stdio"`. Schema confirmed against VS Code's current docs and unit-tested; the server side is exercised by `tests/acceptance/mcp-e2e.sh` on every CI run; VS Code itself reading the file is not yet exercised — see the `TODO(verify)` on `vscode()` in `install.rs`. |
| **GitHub Copilot CLI** (`copilot`) | MCP only, and *global* like Claude Desktop rather than project-scoped: `~/.copilot/mcp-config.json`, with `$COPILOT_HOME` moving the whole directory. A third JSON shape — it pairs Claude Desktop's `mcpServers` top-level key with VS Code's requirement for an explicit `"type"` (`"local"` and `"stdio"` are both accepted; the adapter writes `"stdio"`, the standard MCP name GitHub recommends for configs shared with VS Code and the cloud agent). Because the file is per-machine it carries absolute paths and one `acyclic-<repo name>` entry per registered repo, exactly like Claude Desktop. Schema confirmed against GitHub's current docs and unit-tested; `tests/acceptance/mcp-clients-e2e.sh` drives a real session when `copilot` is on `PATH`. |
| **Copilot coding agent** (cloud) | The outlier: it runs in a GitHub-hosted sandbox, not on the dev's machine, and its MCP servers are configured in the repository's GitHub settings rather than any file in the repo — so there is nothing to merge. `acyclic install copilot-agent` does the two things it usefully can: writes `.github/workflows/copilot-setup-steps.yml` (the documented hook for preparing that sandbox — the job name is load-bearing, and the workflow only takes effect once it is on the default branch) so the binary is installed and `acyclic init` has run before the agent starts, and prints the `mcpServers` JSON to paste into the repo's Copilot settings. The honest caveat, stated in the install output: checkpoints the agent takes live in that sandbox's timeline and do not reach the developer's local one. An existing setup workflow that we did not write is left alone rather than overwritten. Not verified against a real run. |

### More hosts (open TODO)

The two adapter shapes above — lifecycle hooks, and JSON-based MCP registration via `merge_mcp_server_json` — generalize to most other coding-agent CLIs and desktop apps, not just the nine covered so far. See the `TODO(more hosts)` block directly above `run()` in `crates/acyclic/src/install.rs` for the process to follow and the concrete next candidates: **Kimi Code CLI** (has both TOML-based lifecycle hooks and MCP support — needs its own config-file/schema check, not yet done), **Windsurf**, **Zed**, **JetBrains AI assistants**, **Gemini CLI**, **Amazon Q Developer** — all unresearched as of this writing, likely MCP-capable but with unverified config shapes. **Codex's own MCP path** is a separate open item: its config is TOML (`[mcp_servers.<name>]` in `config.toml`), not JSON, so it needs its own writer rather than reusing `merge_mcp_server_json` — see the `TODO(desktop/IDE parity)` on `codex()`, which also flags that Codex's hooks may already cover its desktop app and IDE extension for free, since OpenAI's docs say they share the same config file.

## Configuration

- Per-repo: `.acyclic/config.toml` — checked in, so teams share policy: checkpoint granularity, guarded paths, dry-run default, store size caps, retention TTLs.
- Per-machine: `~/.config/acyclic/` — defaults.
- Zero config is a supported state — defaults are safe everywhere.

## Verb set

The same verbs everywhere, used by both the dev and the agent:

```
acyclic checkpoint [-m msg]      # snapshot now (hooks do this automatically)
acyclic timeline                 # checkpoints, linked to conversation turns
acyclic turns                    # which prompt caused which checkpoints   (Launch 2)
acyclic show <checkpoint>        # session, turn, prompt of one checkpoint  (Launch 2)
acyclic brief                    # previous session's end state + abandoned branches (Launch 2)
acyclic rewind <checkpoint>      # restore the tree, untracked files included
acyclic restore <checkpoint> <p> # restore one path, leave the rest        (Launch 2)
acyclic diff [<a> <b>|--turn N]  # blast radius since session start, between two points, or of one turn
acyclic fork [-n N]              # N copy-on-write working trees        (Launch 3)
acyclic promote <fork>           # merge the winning fork back          (Launch 3)
acyclic search <query>           # indexed search, checkpoint-aware     (Launch 5)
acyclic purge <pattern>          # remove content from ALL checkpoints  (compliance)
```

In hosts with an adapter these surface natively — `/rewind` in Claude Code rather than a shell command — and the agent reaches them as tools, so "try that again a different way" becomes a rollback plus a fresh attempt without the dev naming a checkpoint.

## Claude Desktop: ship decision

The release blocker on the Claude Desktop adapter (above) asked three questions before promoting it from "built, testable" to "documented, supported." Answered:

1. **Is there a non-MCP local-tool mechanism for Claude Desktop?** No. As of the current MCP spec (2026-07-28) and Anthropic's own Desktop Extensions docs, MCP — local (stdio) servers, optionally packaged as a one-click `.mcpb` bundle — is the only third-party extension point Desktop exposes for local tools; there is no separate lifecycle-hook API comparable to Claude Code's, and none is announced. This was true when the plan was drafted and is still true now.
2. **Can one server serve every repo, instead of one registration per repo?** Not with the current design: `acyclic mcp --repo <path>` binds one server process to one repo root at registration time (Desktop starts the subprocess with fixed `args`), so a dev working across N repos needs N `acyclic install claude-desktop` runs and N `mcpServers` entries. A single global server that resolves "which repo" from conversation context isn't possible today — MCP tool calls carry no notion of "the workspace the user has open" the way an IDE extension would. This is real friction versus the CLI adapters (one `acyclic install` per repo, but that's a one-time file the team already checks in) and versus IDE-integrated hosts. Accepted for v1: most users work from a small number of repos, and re-running one install command per repo is annoying, not broken.
3. **What does `.mcpb` packaging buy over the hand-merged config?** Removes the need to hand-edit `claude_desktop_config.json` (Desktop's installer merges it), and is Anthropic's own supported distribution format — but doesn't change the per-repo registration friction from (2), and adds a build/sign step to the release pipeline. Worth doing before broad distribution; not worth blocking on for the current per-machine, per-repo `acyclic install claude-desktop` flow this plan ships.

**Decision: ship the MCP adapter as designed**, with the per-repo registration friction called out in the README/install docs (already done, above) rather than hidden. Revisit `.mcpb` packaging and a possibly-shared server before actively promoting Claude Desktop as a first-class, equally-easy host alongside Claude Code/Codex/Cursor.

## Open questions (not yet settled)

Grouped by what they block. Distribution questions gate everything downstream
of them, so they come first.

### Distribution

1. **Is npm the canonical channel or the stopgap?** If canonical, the README
   should lead with it and demote the installer to a note; if a stopgap, the two
   gates below become the priority. *Measured: npm already serves the binaries
   publicly and integrity-checked, so "npm or the installer" may be a false
   choice — both can serve the same artifacts. Current lean: canonical for now.*
2. ~~**Do we make this repo public?**~~ **Settled 2026-09-17: yes.** This
   unblocks questions 1, 4, 5 and 8, the curl installer's script URL, the SLSA
   and SBOM attestations, and the "verifiable open source" claim in
   `07-compliance.md`. Two things to do before flipping it, neither a
   distribution concern: the git history becomes public along with the tree,
   and `scripts/check-no-secrets.sh` guards the current state rather than
   history; and the compliance claims had to be corrected first, which they now
   are. Distribution was never blocked on this — the binaries have been public
   via npm since 0.0.1.
3. **What version does the first release carry?** `0.0.1` is burned on npm and
   the publish job skips versions already on the registry. *Current lean: 0.0.2
   if the release is a mechanical proof, 0.1.0 if it is the first one anyone is
   told about.*
4. **Do we host `install.sh` on `acyclic.dev`?** It clears gate 1 for the
   script without publishing anything. The original framing of this question
   assumed it "only helps paired with a public binary host" — that assumption
   was wrong, see question 9: the binaries are already on one. *Current lean:
   yes, and it no longer depends on 2.*
5. **Do we notarize macOS binaries?** Costs an Apple Developer account and a
   signing step in the release pipeline. Buys the browser-download route and
   removes a scary dialog. *Measured: there is no dialog. A quarantined binary
   is SIGKILLed with both streams empty and exit 137, so the user gets silence
   rather than an explanation. Current lean: no for v1, and say plainly that
   the browser route is unsupported.*
6. **Do we enforce the glibc floor at install time?** A postinstall check turns
   a linker error into a sentence, but npm postinstall scripts are widely
   disabled (`--ignore-scripts`), and the release pipeline deliberately disables
   them for publishing. *Current lean: a musl or static build erases the problem
   instead of reporting it — prefer that.*
7. **Do we ship a static/musl Linux target?** Removes the libc floor entirely
   and makes the binary work on any distro and in `scratch` containers. Costs a
   fifth build and a second Linux code path to test. *Current lean: yes,
   eventually; it is the real fix for 6.*
8. **Homebrew, crates.io, WSL2 — build, document, or drop?** All three appear in
   docs as if they exist. *Current lean: drop the claims now, build Homebrew
   only after 2 is settled, document WSL2 as untested.*

9. **Should `install.sh` fetch from the npm registry rather than a GitHub
   release?** `registry.npmjs.org` is public, versioned, integrity-checked with
   sha512, already carries every platform binary, and needs no gate opened. The
   costs: a JSON packument parse in POSIX `sh` where today there is a flat
   `SHA256SUMS`; a dependency on npm as infrastructure for a channel whose
   selling point is not needing npm; and the SLSA and SBOM attestations live on
   the GitHub release, so this channel would verify integrity but not
   provenance. *No lean yet — this only became a question when the registry was
   measured to be public.*

### Onboarding

10. **Does `init` become interactive, or does a separate `acyclic setup` own it?**
   Overloading `init` risks surprising scripts that already call it. *Current
   lean: `init` stays mechanical; `setup` is the interactive one, and `init`
   prints a one-line pointer to it.*
11. **Does detection auto-wire or always confirm?** Writing into someone's repo
    unprompted is the fastest way to end a trial. *Current lean: always confirm,
    with `--yes` for scripts.*
12. **Is three questions the right BYOH ceiling?** *Current lean: yes, and treat
    any fourth as a detection failure to fix rather than a question to add.*
13. **Does the secrets scan ship, and is it on by default?** It is the one
    compliance control that ships today (`exclude`), and nobody knows to set it.
    A false positive that silently drops a file from snapshots is the risk.
    *Current lean: scan by default, always ask, never exclude silently.*
14. **Does `install` gain `--auto` and `--dry-run`?** `--dry-run` answers "what
    would it write?", which is the trust moment. *Current lean: both, and
    `--dry-run` before `--auto`.*

### BYOH and host coverage

15. **For an unknown MCP host, do we print a snippet or write their config?**
    Guessing at a config path we have not verified risks corrupting a file we do
    not understand. *Current lean: print, and add a writer only once the shape
    is verified against the real app.*
16. **Does `ACYCLIC_AGENT_CMD` get promoted from the demo kit into the product?**
    *Current lean: yes — it is the only thing that makes "any harness" literal.*
17. **When repo evidence and PATH disagree, which wins?** A repo with `.codex/`
    on a laptop with only `claude` installed is a real case. *Current lean: repo
    evidence wins and PATH becomes a suggestion, because the repo is the shared
    artifact.*
18. **Do we write adapters we cannot verify?** Kimi, Windsurf, Zed, JetBrains,
    Gemini CLI and Amazon Q are all plausibly MCP-capable with unverified config
    shapes, and none is installed locally. *Current lean: no — an unverified
    adapter is worse than an honest gap, and the "Verified" column exists
    precisely to hold this line.*
19. **Codex's MCP path** needs a TOML writer rather than `merge_mcp_server_json`.
    Its hooks may already cover the desktop app and IDE extension for free, since
    the docs say they share `config.toml`. *Current lean: verify the sharing
    claim first — it may make the writer unnecessary.*
20. **OpenCode: upgrade from MCP to its JS lifecycle hooks?** That would move it
    from "the model must remember" to automatic. *Current lean: yes, it is the
    only host where we ship the weaker of two available shapes.*
21. **`.mcpb` packaging and a shared Claude Desktop server** — both deferred in
    the ship decision above, both still open.

## Design commitment

This resolves the mechanism question in favor of **CLI-as-core with host adapters** (not MCP-as-core, not per-host deep builds). That choice is what makes OpenCode and future hosts nearly free. MCP wraps the CLI where a host has no other extension point — Claude Desktop's `acyclic mcp` adapter is exactly that: every MCP tool is a thin translation into the same private `proto::Op` the CLI and hooks already send the daemon, no engine logic lives in the MCP layer itself. See the Claude Desktop row above for why it's marked experimental rather than promoted to a supported host yet.

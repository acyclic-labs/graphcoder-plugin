//! Wire protocol between the `acyclic` CLI/hooks and the per-repo daemon.
//!
//! Transport: newline-delimited JSON over the store's unix socket. One
//! request line yields exactly one response line with the same `id`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// The kinds a client may ask for. Bookkeeping kinds (`baseline`,
/// `noop`, `auto`, ...) exist only on replies: the daemon decides those.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointRequestKind {
    Pre,
    Post,
    Manual,
}

impl CheckpointRequestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pre => "pre",
            Self::Post => "post",
            Self::Manual => "manual",
        }
    }
}

impl FromStr for CheckpointRequestKind {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "pre" => Ok(Self::Pre),
            "post" => Ok(Self::Post),
            "manual" => Ok(Self::Manual),
            other => Err(format!(
                "unknown checkpoint kind {other:?} (pre | post | manual)"
            )),
        }
    }
}

impl fmt::Display for CheckpointRequestKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// Why a checkpoint row exists, as the timeline reports it. Mirrors the
/// engine's `index::CheckpointKind`; the daemon converts between them.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    Baseline,
    Pre,
    Post,
    Manual,
    PreRewind,
    Recovered,
    Failed,
    Noop,
    Auto,
}

impl CheckpointKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Pre => "pre",
            Self::Post => "post",
            Self::Manual => "manual",
            Self::PreRewind => "pre_rewind",
            Self::Recovered => "recovered",
            Self::Failed => "failed",
            Self::Noop => "noop",
            Self::Auto => "auto",
        }
    }
}

impl fmt::Display for CheckpointKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// What a single-path restore did to the path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreAction {
    /// The path now has the checkpoint's contents.
    Restored,
    /// The path was absent at the checkpoint, so it was removed.
    Removed,
}

/// How a path differs between two checkpoints.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
    /// Mode or other metadata only; contents are identical.
    Metadata,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Modified => "modified",
            Self::Metadata => "metadata",
        }
    }

    /// The one-letter tag `diff` listings use: A / D / M, and `m` for
    /// metadata-only, so real blast radius stands out from noise.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Removed => "D",
            Self::Modified => "M",
            Self::Metadata => "m",
        }
    }
}

impl fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    /// Protocol version; mismatches are rejected.
    pub v: u32,
    /// Caller-chosen correlation id, echoed in the response.
    pub id: u64,
    #[serde(flatten)]
    pub op: Op,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Ping,
    Status,
    Checkpoint {
        kind: CheckpointRequestKind,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        tool_call_id: Option<String>,
        #[serde(default)]
        tool_name: Option<String>,
        #[serde(default)]
        label: Option<String>,
        /// true: reply after the checkpoint lands (`PreToolUse` / --wait).
        /// false: reply on enqueue (`PostToolUse` hook path).
        #[serde(default)]
        wait: bool,
        /// Also publish to the authority (coarse boundary).
        #[serde(default)]
        durable: bool,
    },
    Timeline {
        #[serde(default)]
        session_id: Option<String>,
        /// Only checkpoints of this conversation turn (needs `session_id`).
        #[serde(default)]
        turn: Option<i64>,
        #[serde(default = "default_limit")]
        limit: u32,
    },
    /// Conversation turns (the `UserPromptSubmit` hook records them), with
    /// each turn's checkpoint range. All sessions when `session_id` is None.
    Turns {
        #[serde(default)]
        session_id: Option<String>,
    },
    /// Records the start of a conversation turn (hook use).
    TurnStart {
        session_id: String,
        #[serde(default)]
        prompt: String,
    },
    /// One checkpoint, resolved to its session, turn, and prompt.
    Inspect {
        checkpoint: i64,
    },
    /// Sessions newest-first.
    Sessions {
        #[serde(default = "default_limit")]
        limit: u32,
    },
    /// Prose describing one conversation turn, produced by the speculation
    /// runner at the turn boundary. Defaults to the most recent completed
    /// turn of the most recent session.
    Summary {
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        turn: Option<i64>,
        /// Milliseconds to wait for a summary already being produced. Zero
        /// (the default) never waits, which is what every hook path wants.
        #[serde(default)]
        wait_ms: u64,
    },
    /// Agent-readable summary of the previous session: where it ended and
    /// which branches were abandoned. `current` is excluded (it is the
    /// session asking, and it has no history yet).
    Brief {
        #[serde(default)]
        current: Option<String>,
    },
    Rewind {
        target: RewindTarget,
        /// Restore just this path instead of the whole tree.
        #[serde(default)]
        path: Option<String>,
    },
    Diff {
        /// Checkpoint row ids; defaults: session start (or baseline) → latest.
        #[serde(default)]
        before: Option<i64>,
        #[serde(default)]
        after: Option<i64>,
        /// Alternatively, generation hex prefixes (what `promote` and
        /// `status` print); resolved to the latest row holding them.
        #[serde(default)]
        before_hex: Option<String>,
        #[serde(default)]
        after_hex: Option<String>,
    },
    SessionStart {
        session_id: String,
        #[serde(default)]
        host: String,
    },
    SessionEnd {
        session_id: String,
    },
    Commit,
    Stop,
    Fork {
        #[serde(default = "default_fork_count")]
        count: u32,
        /// Owning session, for scratch-tree auto-drop on session end.
        #[serde(default)]
        session_id: Option<String>,
    },
    ForkList,
    ForkDrop {
        /// Named `fork` on the wire: the envelope already owns `id`.
        #[serde(rename = "fork")]
        id: String,
    },
    Promote {
        #[serde(rename = "fork")]
        id: String,
    },
    /// Blast radius of a live fork against its base, without landing it.
    ForkDiff {
        #[serde(rename = "fork")]
        id: String,
    },
    /// Safe Mode: forks one checkout and mounts it directly at the repo
    /// root for the session's duration (see `dry_run` in `.acyclic/config.toml`).
    SessionFork {
        session_id: String,
    },
    /// Safe Mode: unmounts the session's shadow mount, commits its overlay,
    /// and returns the resulting diff without touching the real tree yet.
    /// The fork is held pending `SessionApply`/`SessionDiscard`.
    SessionResolve {
        session_id: String,
    },
    /// Safe Mode: applies a `SessionResolve`d session's changes to the real
    /// tree (the deferred half of promote).
    SessionApply {
        session_id: String,
    },
    /// Safe Mode: discards a `SessionResolve`d session without applying it.
    SessionDiscard {
        session_id: String,
    },
}

fn default_fork_count() -> u32 {
    1
}

fn default_limit() -> u32 {
    50
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindTarget {
    Checkpoint(i64),
    Last,
    SessionStart(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(flatten)]
    pub payload: Payload,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Payload {
    Ok(Box<Reply>),
    Err { message: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Pong,
    Unit,
    Status(StatusInfo),
    /// wait:false acknowledgement — the capture is queued, not yet landed.
    Enqueued,
    Checkpoint(CheckpointInfo),
    Timeline(Vec<TimelineEntry>),
    Rewind(RewindInfo),
    /// Single-path restore (`Rewind` with `path`).
    Restore(RestoreInfo),
    Diff(Vec<DiffEntry>),
    Turns(Vec<TurnEntry>),
    Turn(TurnInfo),
    Inspect(InspectInfo),
    Sessions(Vec<SessionEntry>),
    Brief(BriefInfo),
    Summary(SummaryInfo),
    Forks(Vec<ForkEntry>),
    Promote(PromoteInfo),
    /// A `SessionResolve`d session awaiting `SessionApply`/`SessionDiscard`.
    SessionPending(SessionPendingInfo),
}

/// One turn's summary, and where it came from.
#[derive(Debug, Serialize, Deserialize)]
pub struct SummaryInfo {
    pub session_id: String,
    pub turn: i64,
    /// The prompt that caused the turn, as the index recorded it.
    pub prompt: String,
    /// `None` when nothing has been produced for this turn.
    pub text: Option<String>,
    /// "speculated" (it was waiting), "waited" (produced while asking), or
    /// a reason it is unavailable. Callers surface this: a summary is
    /// generated text, and where it came from is part of reading it.
    pub source: String,
    /// How far ahead of the request it landed, when it was waiting.
    #[serde(default)]
    pub lead_ms: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionPendingInfo {
    pub session_id: String,
    pub diff: Vec<DiffEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ForkEntry {
    pub id: String,
    pub path: String,
    /// "mount" (routed native mount) or "copy" (materialized directory).
    #[serde(default = "default_fork_mode")]
    pub mode: String,
    /// Hex of the published generation the fork was cut from.
    pub base: String,
    pub created_at: i64,
    /// Owning session, if this is a scratch tree tied to one.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Paths still carrying conflict markers from a rebased promote; empty
    /// when the fork has no open conflict.
    #[serde(default)]
    pub conflict_paths: Vec<String>,
    /// The open conflict's three generations (hex), when one is open.
    #[serde(default)]
    pub conflict: Option<ConflictInfo>,
}

/// An open conflict on a fork: the generations it was judged between.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictInfo {
    pub base: String,
    /// The fork's snapshot at the conflicting promote.
    pub ours: String,
    /// The mainline head the fork was rebased onto.
    pub theirs: String,
}

/// One file a promote could not merge cleanly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictEntry {
    pub path: String,
    /// "3 conflicting hunk(s)", "mainline deleted, fork modified", ...
    pub detail: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PromoteInfo {
    pub generation: String,
    /// Where the replaced tree went; absent when the fork had no writes or
    /// when the fork was replayed path by path onto a moved mainline.
    pub old_tree: Option<String>,
    pub warning: String,
    /// Paths written in place because the mainline moved past the fork's
    /// base and nothing overlapped. 0 for a plain swap or a no-op.
    #[serde(default)]
    pub replayed_paths: u32,
    /// Files whose content was merged three-way before landing.
    #[serde(default)]
    pub merged_files: u32,
    /// Non-empty when nothing landed: the fork was rebased onto the current
    /// head and these files carry conflict markers in the fork workspace.
    #[serde(default)]
    pub conflicts: Vec<ConflictEntry>,
    /// The fork workspace to resolve conflicts in (set with `conflicts`).
    #[serde(default)]
    pub fork_path: Option<String>,
    /// Gitignored paths both sides changed: never merged or conflicted,
    /// the mainline's copy was kept (and written into the fork on a rebase).
    #[serde(default)]
    pub kept_mainline: Vec<String>,
    /// Whether the mainline had moved past the fork's base (the fork's
    /// paths were merged onto it) or not (they were written as-is).
    #[serde(default)]
    pub mainline_moved: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusInfo {
    pub state: String,
    pub last_checkpoint: Option<i64>,
    pub unpublished: u64,
    pub store_bytes: u64,
    pub repo_root: String,
    /// Mount provider this daemon would use for forks and Safe Mode.
    #[serde(default)]
    pub mount_provider: String,
    #[serde(default)]
    pub mount_available: bool,
    /// Why mounts are unavailable, when they are.
    #[serde(default)]
    pub mount_reason: Option<String>,
    /// Speculation, when it is configured at all. `None` — the default —
    /// means the daemon prints exactly what it printed before the feature
    /// existed; several acceptance scripts read this output.
    #[serde(default)]
    pub speculate: Option<SpecStatus>,
    /// Native watcher health since the daemon started. `None` from a daemon
    /// older than this field.
    #[serde(default)]
    pub watcher: Option<WatcherStatus>,
}

/// How often the native watcher lost its epoch and what the recoveries
/// cost. Every invalidation is a full-tree rescan, which is where slow
/// checkpoints and promotes come from.
#[derive(Debug, Serialize, Deserialize)]
pub struct WatcherStatus {
    pub invalidations: u32,
    pub last_reason: Option<String>,
    pub recovery_rescans: u32,
    pub recovery_ms_total: f64,
    pub last_recovery_ms: f64,
}

/// What speculation has been doing, over the last 24 hours.
///
/// Reports bytes and run counts rather than money: the daemon cannot know
/// anyone's pricing and should not pretend to. `claimed` against `missed` is
/// the number that says whether speculating is paying off at all.
#[derive(Debug, Serialize, Deserialize)]
pub struct SpecStatus {
    /// Whether this configuration can run a model, and so spend.
    pub spends_tokens: bool,
    /// How the model is invoked, for the status line. Empty when nothing
    /// paid is configured.
    pub command: String,
    pub runs: u64,
    pub claimed: u64,
    pub missed: u64,
    pub timeouts: u64,
    pub bytes_out: u64,
    /// How far ahead of the request a claimed result landed. Near zero means
    /// the trigger is firing too late to be worth anything.
    pub median_lead_ms: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CheckpointInfo {
    pub row_id: i64,
    pub generation: String,
    pub kind: CheckpointKind,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub id: i64,
    pub created_at: i64,
    pub kind: CheckpointKind,
    pub published: bool,
    pub session_id: Option<String>,
    pub tool_name: Option<String>,
    pub label: Option<String>,
    pub error: Option<String>,
    /// Conversation turn within the session, when the host reported one.
    #[serde(default)]
    pub turn: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TurnEntry {
    pub session_id: String,
    pub turn: i64,
    pub started_at: i64,
    pub prompt: String,
    pub first_checkpoint: Option<i64>,
    pub last_checkpoint: Option<i64>,
    pub checkpoints: i64,
    /// Latest real checkpoint before the turn's first one: the diff base
    /// for "what did this turn change".
    pub base_checkpoint: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TurnInfo {
    pub session_id: String,
    pub turn: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InspectInfo {
    pub id: i64,
    pub generation: String,
    pub created_at: i64,
    pub kind: CheckpointKind,
    pub published: bool,
    pub session_id: Option<String>,
    pub host: Option<String>,
    pub turn: Option<i64>,
    pub prompt: Option<String>,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub label: Option<String>,
    pub error: Option<String>,
    pub rewind_target: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionEntry {
    pub session_id: String,
    pub host: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub checkpoints: i64,
    pub turns: i64,
    pub end_checkpoint: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RestoreInfo {
    pub checkpoint: i64,
    pub path: String,
    pub action: RestoreAction,
    /// The `manual` checkpoint recording the tree after the restore.
    pub recorded_checkpoint: Option<i64>,
}

/// The previous-session summary. Structured so hosts can render it; the CLI
/// renders it as < 1KB of text for the agent's context.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct BriefInfo {
    pub session: Option<BriefSession>,
    /// Files that differ between the previous session's end state and the
    /// latest checkpoint (edits made outside any session, or by a later
    /// session that left no end state).
    pub drift_files: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BriefSession {
    pub session_id: String,
    pub host: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub turns: i64,
    pub checkpoints: i64,
    pub end_checkpoint: Option<i64>,
    pub end_turn: Option<i64>,
    pub end_prompt: Option<String>,
    /// Files changed from the session's first checkpoint to its last.
    pub files_changed: u64,
    pub sample_paths: Vec<String>,
    /// Prose for the session's last turn, when speculation produced one.
    /// Looked up fresh rather than stored in the brief: whether a summary
    /// exists is independent of everything else the brief reports.
    #[serde(default)]
    pub summary: Option<String>,
    pub abandoned: Vec<BriefAbandoned>,
}

/// One rewind taken during the session: the checkpoints it left behind.
#[derive(Debug, Serialize, Deserialize)]
pub struct BriefAbandoned {
    /// First and last abandoned checkpoint ids.
    pub from_checkpoint: i64,
    pub to_checkpoint: i64,
    /// Where the rewind went back to.
    pub rewound_to: i64,
    pub turn: Option<i64>,
    pub prompt: Option<String>,
    pub checkpoints: i64,
    pub files_changed: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RewindInfo {
    pub restored_checkpoint: i64,
    pub old_tree: String,
    pub warning: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DiffEntry {
    pub path: String,
    pub change: ChangeKind,
    pub file_kind: String,
    /// Matched by the repo's `.gitignore` (caches, build output, secrets):
    /// shown, since rewind restores it, but not blast radius.
    #[serde(default)]
    pub ignored: bool,
}

fn default_fork_mode() -> String {
    "mount".to_owned()
}

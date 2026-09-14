//! Wire protocol between the `acyclic` CLI/hooks and the per-repo daemon.
//!
//! Transport: newline-delimited JSON over the store's unix socket. One
//! request line yields exactly one response line with the same `id`.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

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
        /// "pre" | "post" | "manual"
        kind: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        tool_call_id: Option<String>,
        #[serde(default)]
        tool_name: Option<String>,
        #[serde(default)]
        label: Option<String>,
        /// true: reply after the checkpoint lands (PreToolUse / --wait).
        /// false: reply on enqueue (PostToolUse hook path).
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
    Forks(Vec<ForkEntry>),
    Promote(PromoteInfo),
    /// A `SessionResolve`d session awaiting `SessionApply`/`SessionDiscard`.
    SessionPending(SessionPendingInfo),
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
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CheckpointInfo {
    pub row_id: i64,
    pub generation: String,
    pub kind: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub id: i64,
    pub created_at: i64,
    pub kind: String,
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
    pub kind: String,
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
    /// "restored" | "removed"
    pub action: String,
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
    /// "added" | "removed" | "modified" | "metadata"
    pub change: String,
    pub file_kind: String,
    /// Matched by the repo's `.gitignore` (caches, build output, secrets):
    /// shown, since rewind restores it, but not blast radius.
    #[serde(default)]
    pub ignored: bool,
}

fn default_fork_mode() -> String {
    "mount".to_string()
}

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
        #[serde(default = "default_limit")]
        limit: u32,
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
    Ok(Reply),
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
    Diff(Vec<DiffEntry>),
    Forks(Vec<ForkEntry>),
    Promote(PromoteInfo),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ForkEntry {
    pub id: String,
    pub path: String,
    /// Hex of the published generation the fork was cut from.
    pub base: String,
    pub created_at: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PromoteInfo {
    pub generation: String,
    /// Where the replaced tree went; absent when the fork had no writes.
    pub old_tree: Option<String>,
    pub warning: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusInfo {
    pub state: String,
    pub last_checkpoint: Option<i64>,
    pub unpublished: u64,
    pub store_bytes: u64,
    pub repo_root: String,
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
}

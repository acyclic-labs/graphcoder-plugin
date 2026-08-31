//! Host-hook entrypoint. Claude Code (and compatible hosts) invoke
//! `acyclic hook <event>` with a JSON payload on stdin. The contract:
//! NEVER block or fail the agent — every path exits 0, a missing daemon is
//! a silent no-op, and pre-tool waits are bounded.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use acyclic_proto as proto;

use crate::client::{Client, ConnectError, Spawn};

/// Claude Code hook payload (superset-tolerant: unknown fields ignored).
#[derive(Debug, serde::Deserialize, Default)]
struct Payload {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
}

/// The bound on a pre-tool wait: an exact boundary is nice to have, but the
/// agent's latency matters more. Past this, the capture still lands (FIFO),
/// just labeled by enqueue order rather than a strict barrier.
const PRE_TOOL_WAIT: Duration = Duration::from_millis(2_000);

pub fn run(repo: &Path, event: &str) -> i32 {
    // Reading stdin can't hang the agent: hosts close it after writing.
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let payload: Payload = serde_json::from_str(&raw).unwrap_or_default();

    let Ok(mut client) = connect(repo) else {
        // No daemon (not initialized, or stopped): checkpointing is off.
        // Stay quiet — hooks fire on every tool call.
        return 0;
    };

    let op = match event {
        "pre-tool" => proto::Op::Checkpoint {
            kind: "pre".into(),
            session_id: payload.session_id,
            tool_call_id: payload.tool_use_id,
            tool_name: payload.tool_name,
            label: None,
            wait: true,
            durable: false,
        },
        "post-tool" => proto::Op::Checkpoint {
            kind: "post".into(),
            session_id: payload.session_id,
            tool_call_id: payload.tool_use_id,
            tool_name: payload.tool_name,
            label: None,
            wait: false,
            durable: false,
        },
        "session-start" => proto::Op::SessionStart {
            session_id: payload.session_id.unwrap_or_default(),
            host: "claude-code".into(),
        },
        "session-end" => proto::Op::SessionEnd {
            session_id: payload.session_id.unwrap_or_default(),
        },
        other => {
            eprintln!("acyclic hook: unknown event {other:?}");
            return 0;
        }
    };

    let bounded = matches!(event, "pre-tool");
    if bounded {
        client.set_deadline(PRE_TOOL_WAIT);
    }
    if let Err(message) = client.call(op) {
        // Deadline overruns and daemon hiccups are advisory only.
        eprintln!("acyclic hook ({event}): {message}");
    }
    0
}

fn connect(repo: &Path) -> Result<Client, ConnectError> {
    let paths = crate::store_paths(repo).map_err(ConnectError::Other)?;
    let log = paths.root.join("daemon.log");
    Client::connect(&paths.socket(), repo, &log, Spawn::Never)
}
